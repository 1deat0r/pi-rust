//! Public TUI controller surfaces corresponding to upstream `TuiBase`,
//! `TuiMainScreen`, and `TuiAltScreen`.
//!
//! The controller owns terminal lifecycle, focus, overlays, retained layout,
//! and deterministic frame rendering.  It intentionally accepts raw strings
//! at the input boundary so existing consumers can keep parsing keyboard
//! sequences, while typed mouse reports are decoded before key dispatch.

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use crate::components::alt_screen::{
    extract_selection, find_alt_screen_search_matches, AltScreenSearchComponent, SearchMatch,
    SelectionPoint, SelectionRange,
};
use crate::keybindings::get_keybindings;
use crate::keys::{is_key_release, parse_key, TuiKey};
use crate::layout::{get_scrollbar_geometry, LayoutFrame, ScrollbarGeometry};
use crate::mouse::{decode_mouse_event, MouseButton, MouseEvent, MouseEventKind};
use crate::terminal::{
    TerminalBackend, TerminalEvent, BEGIN_SYNC_UPDATE, CLEAR_SCREEN_HOME, DISABLE_AUTOWRAP,
    ENABLE_AUTOWRAP, END_SYNC_UPDATE, HIDE_CURSOR, SHOW_CURSOR,
};
use crate::tui::{
    Component, Container, OverlayHandle, OverlayManager, OverlayOptions, SharedComponent,
    CURSOR_MARKER,
};
use crate::utils::{
    extract_ansi_code, normalize_terminal_output, slice_by_column, slice_by_column_strict,
    visible_width,
};

const PAGE_SCROLL_OVERLAP: usize = 4;
const MAX_CACHED_OFFSCREEN_KITTY_IMAGES: usize = 16;
const MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES: u64 = 64 * 1024 * 1024;

/// Thread-safe repaint signal used to bridge retained components and the
/// event loop that owns a controller.  The callback is a wake-up hook only;
/// timer threads must not render or touch terminal state directly.
pub type RequestRenderCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct RenderRequestState {
    requested: bool,
    force: bool,
}

#[derive(Clone)]
struct RenderInvalidation {
    state: Arc<Mutex<RenderRequestState>>,
    callback: Arc<Mutex<Option<RequestRenderCallback>>>,
    request_callback: Arc<OnceLock<RequestRenderCallback>>,
}

impl RenderInvalidation {
    fn request_state(
        state: &Arc<Mutex<RenderRequestState>>,
        callback_slot: &Arc<Mutex<Option<RequestRenderCallback>>>,
        force: bool,
    ) {
        let notify = {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            let notify = force || !state.requested;
            state.requested = true;
            state.force |= force;
            notify
        };
        if !notify {
            return;
        }
        let callback = callback_slot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(callback) = callback {
            // A host wake-up hook is an integration boundary. Keep a faulty
            // embedding callback from unwinding through a component timer.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback();
            }));
        }
    }

    fn set_callback(&self, callback: Option<RequestRenderCallback>) {
        *self
            .callback
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = callback;
    }

    fn request(&self, force: bool) {
        Self::request_state(&self.state, &self.callback, force);
    }

    fn take(&self) -> Option<bool> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.requested {
            return None;
        }
        state.requested = false;
        Some(std::mem::take(&mut state.force))
    }

    fn is_requested(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requested
    }

    fn callback(&self) -> RequestRenderCallback {
        self.request_callback
            .get_or_init(|| {
                let state = Arc::clone(&self.state);
                let callback = Arc::clone(&self.callback);
                Arc::new(move || Self::request_state(&state, &callback, false))
            })
            .clone()
    }
}

impl Default for RenderInvalidation {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(RenderRequestState::default())),
            callback: Arc::new(Mutex::new(None)),
            request_callback: Arc::new(OnceLock::new()),
        }
    }
}

/// Whether a controller renders into scrollback or the alternate screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    Regular,
    Fullscreen,
}

/// Stop behavior shared by both controller surfaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TuiStopOptions {
    pub preserve_screen: bool,
}

/// A simple input listener handle. Dropping it removes the listener.
pub struct InputListenerHandle {
    id: usize,
    listeners: Arc<Mutex<Vec<(usize, InputListener)>>>,
}

impl Drop for InputListenerHandle {
    fn drop(&mut self) {
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.retain(|(id, _)| *id != self.id);
        }
    }
}

type InputListener = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedKittyImage {
    transmission_generation: u64,
    transmission_bytes: usize,
    estimated_decoded_bytes: u64,
}

struct PreparedKittyScreen {
    lines: Vec<String>,
    evicted_image_deletion: String,
    stale_image_deletion: String,
}

struct ControllerCore {
    terminal: Arc<Mutex<TerminalBackend>>,
    root: Arc<Mutex<Container>>,
    render_invalidation: RenderInvalidation,
    overlays: OverlayManager,
    focused: Option<SharedComponent>,
    listeners: Arc<Mutex<Vec<(usize, InputListener)>>>,
    next_listener: usize,
    started: bool,
    suspended: bool,
    mode: TuiMode,
    show_hardware_cursor: bool,
    clear_on_shrink: bool,
    full_redraws: usize,
    previous_lines: Vec<String>,
    max_lines_rendered: usize,
    previous_width: usize,
    previous_height: usize,
    uploaded_kitty_images: Vec<(u32, CachedKittyImage)>,
    last_frame: Option<LayoutFrame>,
    last_cursor_position: Option<(usize, usize)>,
    last_cursor_visible: Option<bool>,
}

impl ControllerCore {
    fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            terminal,
            root: Arc::new(Mutex::new(Container::new())),
            render_invalidation: RenderInvalidation::default(),
            overlays: OverlayManager::new(),
            focused: None,
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener: 0,
            started: false,
            suspended: false,
            mode: TuiMode::Regular,
            show_hardware_cursor: std::env::var("PI_HARDWARE_CURSOR").ok().as_deref() == Some("1"),
            clear_on_shrink: std::env::var("PI_CLEAR_ON_SHRINK").ok().as_deref() == Some("1"),
            full_redraws: 0,
            previous_lines: Vec::new(),
            max_lines_rendered: 0,
            previous_width: 0,
            previous_height: 0,
            uploaded_kitty_images: Vec::new(),
            last_frame: None,
            last_cursor_position: None,
            last_cursor_visible: None,
        }
    }

    fn add_child(&mut self, component: SharedComponent) {
        self.root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .add_child(component);
    }

    fn remove_child(&mut self, component: &SharedComponent) -> bool {
        self.root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove_child(component)
    }

    fn clear(&mut self) {
        self.root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.focused = None;
    }

    fn invalidate_components(&mut self) {
        self.root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate();
        self.overlays.invalidate();
    }

    fn set_focus(&mut self, component: Option<SharedComponent>) {
        if let Some(previous) = &self.focused {
            previous
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_focused(false);
        }
        if let Some(next) = &component {
            next.lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_focused(true);
        }
        self.focused = component;
    }

    fn add_input_listener(
        &mut self,
        listener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> InputListenerHandle {
        let id = self.next_listener;
        self.next_listener = self.next_listener.saturating_add(1);
        let listeners = self.listeners.clone();
        listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((id, Arc::new(listener)));
        InputListenerHandle { id, listeners }
    }

    fn dispatch_mouse(&mut self, event: &MouseEvent) {
        if self.overlays.has_visible_overlay() {
            self.overlays.dispatch_mouse(event);
        } else if let Some(focused) = &self.focused {
            focused
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .handle_mouse(event);
        }
    }

    fn dispatch_key(&mut self, key: &TuiKey) {
        if key.base == "z" && key.ctrl && !key.alt && !key.shift && self.started {
            let _ = self.suspend();
            return;
        }
        if self.overlays.has_visible_overlay() {
            self.overlays.dispatch(key);
        } else if let Some(focused) = &self.focused {
            focused
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .handle_input(key);
        }
    }

    fn dispatch_raw(&mut self, raw: &str) -> bool {
        if self.listener_consumes(raw) {
            return false;
        }
        if self.consume_cell_size_response(raw) {
            return false;
        }
        self.dispatch_raw_after_listeners(raw);
        true
    }

    fn consume_cell_size_response(&mut self, raw: &str) -> bool {
        let updates_dimensions = crate::terminal_image::parse_cell_size_response(raw).is_some();
        let consumed = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .consume_cell_size_response(raw);
        if consumed && updates_dimensions {
            self.invalidate_components();
            self.request_render(false);
        }
        consumed
    }

    fn dispatch_raw_after_listeners(&mut self, raw: &str) {
        // Upstream drops Kitty key-release events before focused components
        // unless they explicitly opt in. Rust components currently have no
        // release-event opt-in, so filter them at the shared raw boundary to
        // avoid duplicate selection moves or printable release insertion.
        if is_key_release(raw) {
            return;
        }
        if let Ok(Some(event)) = decode_mouse_event(raw) {
            self.dispatch_mouse(&event);
        } else {
            self.dispatch_key(&parse_key(raw));
        }
    }

    fn listener_consumes(&self, raw: &str) -> bool {
        let listeners = self
            .listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        listeners.iter().any(|(_, listener)| listener(raw))
    }

    fn set_request_render_callback(&self, callback: Option<RequestRenderCallback>) {
        self.render_invalidation.set_callback(callback);
    }

    fn request_render(&self, force: bool) {
        self.render_invalidation.request(force);
    }

    fn take_render_request(&self) -> Option<bool> {
        self.render_invalidation.take()
    }

    fn is_render_requested(&self) -> bool {
        self.render_invalidation.is_requested()
    }

    fn request_render_callback(&self) -> RequestRenderCallback {
        self.render_invalidation.callback()
    }

    /// Replace repeat Kitty transmissions with placement-only commands on the
    /// fullscreen path, retaining recent offscreen uploads for fast re-entry.
    fn prepare_kitty_screen(&mut self, screen: &[String]) -> PreparedKittyScreen {
        let mut visible_image_ids = BTreeSet::new();
        let mut lines = Vec::with_capacity(screen.len());
        let mut stale_image_deletion = String::new();

        for line in screen {
            let Some(placement) = crate::terminal_image::get_kitty_image_placement(line) else {
                lines.push(line.clone());
                continue;
            };
            visible_image_ids.insert(placement.image_id);

            let cached = self
                .uploaded_kitty_images
                .iter()
                .position(|(image_id, _)| *image_id == placement.image_id)
                .map(|index| self.uploaded_kitty_images.remove(index).1);
            let reused = cached.as_ref().is_some_and(|cached| {
                cached.transmission_generation == placement.transmission_generation
            });
            if cached.is_some() && !reused {
                stale_image_deletion.push_str(&crate::terminal_image::delete_kitty_image(
                    placement.image_id,
                ));
            }
            self.uploaded_kitty_images.push((
                placement.image_id,
                CachedKittyImage {
                    transmission_generation: placement.transmission_generation,
                    transmission_bytes: placement.transmission_bytes,
                    estimated_decoded_bytes: placement.estimated_decoded_bytes,
                },
            ));

            if reused {
                lines.push(placement.replacement_line);
            } else {
                lines.push(line.clone());
            }
        }

        let mut offscreen_count = 0usize;
        let mut offscreen_transmission_bytes = 0usize;
        let mut offscreen_decoded_bytes = 0u64;
        for (image_id, cached) in &self.uploaded_kitty_images {
            if visible_image_ids.contains(image_id) {
                continue;
            }
            offscreen_count = offscreen_count.saturating_add(1);
            offscreen_transmission_bytes =
                offscreen_transmission_bytes.saturating_add(cached.transmission_bytes);
            offscreen_decoded_bytes =
                offscreen_decoded_bytes.saturating_add(cached.estimated_decoded_bytes);
        }

        let mut evicted_image_deletion = String::new();
        let mut index = 0;
        while offscreen_count > MAX_CACHED_OFFSCREEN_KITTY_IMAGES
            || offscreen_transmission_bytes > MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
            || offscreen_decoded_bytes > MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
        {
            if index >= self.uploaded_kitty_images.len() {
                break;
            }
            if visible_image_ids.contains(&self.uploaded_kitty_images[index].0) {
                index += 1;
                continue;
            }
            let (image_id, cached) = self.uploaded_kitty_images.remove(index);
            evicted_image_deletion.push_str(&crate::terminal_image::delete_kitty_image(image_id));
            offscreen_count = offscreen_count.saturating_sub(1);
            offscreen_transmission_bytes =
                offscreen_transmission_bytes.saturating_sub(cached.transmission_bytes);
            offscreen_decoded_bytes =
                offscreen_decoded_bytes.saturating_sub(cached.estimated_decoded_bytes);
        }

        PreparedKittyScreen {
            lines,
            evicted_image_deletion,
            stale_image_deletion,
        }
    }

    fn render_component(&mut self, root: SharedComponent, mode: TuiMode, force: bool) {
        self.render_component_with_selection(root, mode, force, None);
    }

    fn render_component_with_selection(
        &mut self,
        root: SharedComponent,
        mode: TuiMode,
        force: bool,
        selection: Option<SelectionRange>,
    ) {
        // `renderNow` consumes the pending request just like upstream
        // TuiBase. A callback fired while layout is being painted remains
        // pending for the next owner-driven frame.
        let requested_force = self.render_invalidation.take().unwrap_or(false);
        let force = force || requested_force;
        let (width, height) = {
            let terminal = self
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (terminal.width().max(1), terminal.height().max(1))
        };
        let mut frame = crate::layout::render_layout_frame_with_request(
            root,
            width,
            height,
            Some(self.render_invalidation.callback()),
        );
        frame.lines = self.overlays.composite(&frame.lines, width, height);
        let mut visible_start = 0;
        if mode == TuiMode::Fullscreen && frame.lines.len() > height {
            visible_start = frame.lines.len() - height;
            frame.lines = frame.lines.split_off(visible_start);
        }
        if let Some(selection) = selection {
            apply_selection_highlight(
                &mut frame.lines,
                &frame.root,
                frame.width,
                selection,
                visible_start,
            );
        }
        let cursor_position = extract_cursor_position(&mut frame.lines, height);
        // `render_layout_frame_with_request` pads its paint buffer to the
        // terminal viewport. Retain the natural allocated extent separately
        // so regular-screen clear-on-shrink can observe a real contraction.
        let rendered_line_count = frame
            .root
            .children
            .iter()
            .map(|child| child.rect.y.saturating_add(child.rect.height))
            .max()
            .unwrap_or_else(|| {
                frame
                    .lines
                    .iter()
                    .rposition(|line| !line.is_empty())
                    .map_or(0, |row| row + 1)
            });
        let mut lines = std::mem::take(&mut frame.lines)
            .into_iter()
            .map(|line| {
                let line = normalize_terminal_output(&line);
                if mode == TuiMode::Regular {
                    line.replace(CURSOR_MARKER, "")
                } else {
                    line
                }
            })
            .collect::<Vec<_>>();
        lines.resize(height, String::new());
        lines.truncate(height);
        let first_regular_frame = mode == TuiMode::Regular && self.previous_lines.is_empty();
        let changed_dimensions = width != self.previous_width || height != self.previous_height;
        let full = force || self.previous_lines.is_empty() || changed_dimensions;
        let clear_required = mode == TuiMode::Regular
            && self.clear_on_shrink
            && rendered_line_count < self.max_lines_rendered
            && !self.overlays.has_visible_overlay();
        // Upstream's regular renderer clears scrollback whenever a full
        // redraw is caused by a resize, forced reflow, or clear-on-shrink.
        // The alternate screen has no scrollback to clear.
        let clear_scrollback = mode == TuiMode::Regular
            && !self.previous_lines.is_empty()
            && (force || changed_dimensions || clear_required);
        let image_rows_changed = mode == TuiMode::Fullscreen
            && (0..self.previous_lines.len().max(lines.len())).any(|row| {
                let previous = self
                    .previous_lines
                    .get(row)
                    .map(String::as_str)
                    .unwrap_or("");
                let current = lines.get(row).map(String::as_str).unwrap_or("");
                previous != current
                    && (crate::terminal_image::is_image_line(previous)
                        || crate::terminal_image::is_image_line(current))
            });
        let image_redraw = mode == TuiMode::Fullscreen && (full || image_rows_changed);
        let has_kitty_image = self
            .previous_lines
            .iter()
            .chain(&lines)
            .any(|line| line.contains("\x1b_G"));
        let has_iterm_image = self
            .previous_lines
            .iter()
            .chain(&lines)
            .any(|line| line.contains("\x1b]1337;File="));
        let kitty_image_redraw = image_redraw && has_kitty_image;
        let iterm_image_redraw = image_redraw && has_iterm_image && !kitty_image_redraw;
        let had_uploaded_kitty_images = !self.uploaded_kitty_images.is_empty();
        let prepared_kitty_screen = if kitty_image_redraw {
            Some(self.prepare_kitty_screen(&lines))
        } else {
            None
        };
        let prepared_lines = prepared_kitty_screen
            .as_ref()
            .map_or(lines.as_slice(), |prepared| prepared.lines.as_slice());
        let content_changed = full
            || clear_required
            || self.previous_lines.len() != lines.len()
            || self
                .previous_lines
                .iter()
                .zip(&lines)
                .any(|(previous, current)| previous != current);
        let cursor_visible = cursor_position.is_some() && self.show_hardware_cursor;
        if !content_changed
            && self.last_cursor_position == cursor_position
            && self.last_cursor_visible == Some(cursor_visible)
        {
            // Layout still ran so hit-testing and overlay state stay current,
            // but avoid emitting a synchronized frame when neither content
            // nor the hardware cursor changed.
            self.max_lines_rendered = self.max_lines_rendered.max(rendered_line_count);
            self.last_frame = Some(frame);
            return;
        }
        if full {
            self.full_redraws = self.full_redraws.saturating_add(1);
        }
        let cursor_only = !content_changed;
        let mut terminal = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut output = String::new();
        if !cursor_only {
            output.push_str(BEGIN_SYNC_UPDATE);
            if mode == TuiMode::Fullscreen
                && image_redraw
                && (kitty_image_redraw || had_uploaded_kitty_images)
            {
                if had_uploaded_kitty_images {
                    output.push_str(crate::terminal_image::delete_all_kitty_placements());
                } else {
                    output.push_str(&kitty_cleanup_sequences(
                        &self.previous_lines,
                        &lines,
                        full || clear_required,
                    ));
                }
                if let Some(prepared) = &prepared_kitty_screen {
                    output.push_str(&prepared.stale_image_deletion);
                    output.push_str(&prepared.evicted_image_deletion);
                }
            } else {
                output.push_str(&kitty_cleanup_sequences(
                    &self.previous_lines,
                    &lines,
                    full || clear_required,
                ));
            }
            if !first_regular_frame {
                if full || clear_required {
                    output.push_str(CLEAR_SCREEN_HOME);
                    if clear_scrollback {
                        output.push_str("\x1b[3J");
                    }
                } else if iterm_image_redraw {
                    output.push_str("\x1b[2J");
                } else {
                    output.push_str("\x1b[H");
                }
            }
            if first_regular_frame {
                let line_count = rendered_line_count.min(prepared_lines.len());
                for (row, line) in prepared_lines.iter().take(line_count).enumerate() {
                    if row > 0 {
                        output.push_str("\r\n");
                    }
                    output.push_str(&clamp_controller_line(line, width));
                }
            } else {
                for (row, line) in prepared_lines.iter().enumerate() {
                    if !full && !image_redraw && self.previous_lines.get(row) == Some(line) {
                        continue;
                    }
                    let rendered_line = clamp_controller_line(line, width);
                    output.push_str(&format!("\x1b[{};1H\x1b[2K{}", row + 1, rendered_line));
                }
            }
        }
        if let Some((row, col)) = cursor_position {
            output.push_str(&format!("\x1b[{};{}H", row + 1, col.min(width) + 1));
            output.push_str(if self.show_hardware_cursor {
                SHOW_CURSOR
            } else {
                HIDE_CURSOR
            });
        } else {
            output.push_str(HIDE_CURSOR);
        }
        if !cursor_only {
            output.push_str(END_SYNC_UPDATE);
        }
        if mode == TuiMode::Regular && !cursor_only {
            output.push('\r');
        }
        terminal.write_raw(&output);
        drop(terminal);
        self.previous_lines = lines;
        if clear_required {
            self.max_lines_rendered = rendered_line_count;
        } else {
            self.max_lines_rendered = self.max_lines_rendered.max(rendered_line_count);
        }
        self.previous_width = width;
        self.previous_height = height;
        self.last_frame = Some(frame);
        self.last_cursor_position = cursor_position;
        self.last_cursor_visible = Some(cursor_visible);
    }

    fn start(&mut self, mode: TuiMode) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        self.mode = mode;
        self.suspended = false;
        self.started = true;
        let use_alt = mode == TuiMode::Fullscreen;
        if let Err(error) = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enter_raw_with_alt_screen(use_alt)
        {
            self.started = false;
            return Err(error);
        }
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .query_cell_size();
        Ok(())
    }

    fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        if !self.started {
            self.suspended = false;
            return Ok(());
        }
        self.started = false;
        if self.mode == TuiMode::Fullscreen {
            // Alt-screen teardown must release the terminal's complete Kitty
            // image store, not only the ids still visible in the retained
            // frame. The current Rust renderer has no separate upload cache,
            // so a visible Kitty command is the evidence that cleanup is
            // needed here.
            let cleanup = if kitty_image_ids(&self.previous_lines).is_empty()
                && self.uploaded_kitty_images.is_empty()
            {
                "".to_string()
            } else {
                crate::terminal_image::delete_all_kitty_images().to_string()
            };
            if !cleanup.is_empty() {
                let mut terminal = self
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let mut output = String::from(BEGIN_SYNC_UPDATE);
                output.push_str(&cleanup);
                output.push_str(END_SYNC_UPDATE);
                terminal.write_raw(&output);
            }
            self.uploaded_kitty_images.clear();
        }
        if !options.preserve_screen {
            if self.mode == TuiMode::Regular {
                // TuiMainScreen leaves its rendered scrollback in place and
                // advances past the final frame before returning control to
                // the shell. Fullscreen teardown has a separate projection
                // path and retains its historical clear behavior here.
                self.terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .write_raw(" \r\n");
            } else {
                self.terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clear_screen();
            }
        }
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .leave_raw()
    }

    fn suspend(&mut self) -> io::Result<()> {
        if !self.started {
            return Ok(());
        }
        let result = self.stop(TuiStopOptions {
            preserve_screen: true,
        });
        if result.is_ok() {
            self.suspended = true;
        }
        result
    }

    fn resume(&mut self) -> io::Result<()> {
        if !self.suspended {
            return Ok(());
        }
        self.start(self.mode)
    }

    fn is_suspended(&self) -> bool {
        self.suspended
    }
}

/// State that can be captured around a main-screen handoff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiMainScreenRenderState {
    pub previous_lines: Vec<String>,
    pub previous_width: usize,
    pub previous_height: usize,
    pub cursor_row: usize,
    pub hardware_cursor_row: usize,
    pub max_lines_rendered: usize,
    pub previous_viewport_top: usize,
}

/// Regular/main-screen controller.
pub struct TuiMainScreen {
    core: ControllerCore,
}

impl TuiMainScreen {
    pub fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            core: ControllerCore::new(terminal),
        }
    }

    pub fn mode(&self) -> TuiMode {
        TuiMode::Regular
    }
    pub fn terminal(&self) -> Arc<Mutex<TerminalBackend>> {
        self.core.terminal.clone()
    }
    pub fn add_child(&mut self, component: SharedComponent) {
        self.core.add_child(component);
    }
    pub fn remove_child(&mut self, component: &SharedComponent) -> bool {
        self.core.remove_child(component)
    }
    pub fn clear(&mut self) {
        self.core.clear();
    }
    pub fn set_focus(&mut self, component: Option<SharedComponent>) {
        self.core.set_focus(component);
    }
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.core.show_hardware_cursor
    }
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        if self.core.show_hardware_cursor == enabled {
            return;
        }
        self.core.show_hardware_cursor = enabled;
        if !enabled {
            self.core
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .hide_cursor();
        }
        // Upstream only invalidates here; the owner-driven render loop must
        // perform layout and terminal writes after the state change.
        self.core.request_render(false);
    }
    pub fn get_clear_on_shrink(&self) -> bool {
        self.core.clear_on_shrink
    }
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.core.clear_on_shrink = enabled;
    }
    pub fn full_redraws(&self) -> usize {
        self.core.full_redraws
    }
    pub fn has_overlay(&self) -> bool {
        self.core.overlays.has_visible_overlay()
    }
    pub fn show_overlay(
        &mut self,
        component: SharedComponent,
        options: OverlayOptions,
    ) -> OverlayHandle {
        let handle = self.core.overlays.show_overlay(component, options);
        // Match TuiBase.showOverlay: modal presentation hides the hardware
        // cursor immediately, while the owner performs the next full frame.
        self.core
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .hide_cursor();
        self.core.request_render(false);
        handle
    }
    pub fn hide_overlay(&mut self, handle: OverlayHandle) -> bool {
        let hidden = self.core.overlays.hide(handle);
        if hidden {
            if !self.core.overlays.has_visible_overlay() {
                self.core
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .hide_cursor();
            }
            self.core.request_render(false);
        }
        hidden
    }
    pub fn add_input_listener(
        &mut self,
        listener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> InputListenerHandle {
        self.core.add_input_listener(listener)
    }
    /// Install a host wake-up hook for timer-driven or asynchronous repaint
    /// requests. The hook must only wake the owner's event loop; rendering
    /// remains on that loop's thread.
    pub fn set_request_render_callback(&mut self, callback: Option<RequestRenderCallback>) {
        self.core.set_request_render_callback(callback);
    }
    /// Request a repaint without rendering synchronously. The returned force
    /// bit from [`Self::take_render_request`] lets an owner preserve upstream
    /// `requestRender(true)` semantics.
    pub fn request_render(&self, force: bool) {
        self.core.request_render(force);
    }
    /// Consume one coalesced repaint request, returning whether it requires a
    /// full render-state reset.
    pub fn take_render_request(&self) -> Option<bool> {
        self.core.take_render_request()
    }
    /// Check whether the controller has a repaint waiting for its owner.
    pub fn is_render_requested(&self) -> bool {
        self.core.is_render_requested()
    }
    /// Obtain the callback that can be passed to animated components such as
    /// `LoaderOptions` so their timer updates share this controller's wake-up
    /// and coalescing path.
    pub fn request_render_callback(&self) -> RequestRenderCallback {
        self.core.request_render_callback()
    }
    pub fn dispatch_raw(&mut self, raw: &str) {
        if self.core.dispatch_raw(raw) && self.core.started {
            // Match upstream TUI input handling: input dispatch only queues a
            // repaint. The owner decides when to render, so layout and the
            // terminal write never sit on the per-keystroke input path.
            self.core.request_render(false);
        }
    }
    pub fn dispatch_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::Key(raw) => self.dispatch_raw(&raw),
            TerminalEvent::Resize(width, height) => {
                self.core
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .set_size(width, height);
                // Match upstream TuiBase's resize callback: queue a normal
                // owner-driven repaint instead of rendering while the input
                // event is being dispatched. The owner can coalesce this with
                // adjacent resize notifications and input updates.
                self.core.request_render(false);
            }
        }
    }
    pub fn start(&mut self) -> io::Result<()> {
        let result = self.core.start(TuiMode::Regular);
        if result.is_ok() {
            self.render_now(true);
        }
        result
    }
    pub fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        self.core.stop(options)
    }
    /// Temporarily leave raw mode so the caller can suspend the process.
    /// Call [`Self::resume`] after SIGCONT to restore the TUI.
    pub fn suspend(&mut self) -> io::Result<()> {
        self.core.suspend()
    }
    pub fn resume(&mut self) -> io::Result<()> {
        let was_suspended = self.core.is_suspended();
        let result = self.core.resume();
        if result.is_ok() && was_suspended {
            self.render_now(true);
        }
        result
    }
    pub fn is_suspended(&self) -> bool {
        self.core.is_suspended()
    }
    pub fn render_now(&mut self, force: bool) {
        self.core
            .render_component(self.core.root.clone(), TuiMode::Regular, force);
    }
    pub fn capture_render_state(&self) -> TuiMainScreenRenderState {
        TuiMainScreenRenderState {
            previous_lines: self.core.previous_lines.clone(),
            previous_width: self.core.previous_width,
            previous_height: self.core.previous_height,
            cursor_row: self.core.previous_lines.len().saturating_sub(1),
            hardware_cursor_row: self.core.previous_lines.len().saturating_sub(1),
            max_lines_rendered: self.core.max_lines_rendered,
            previous_viewport_top: 0,
        }
    }
    pub fn restore_render_state(&mut self, state: TuiMainScreenRenderState) {
        // Match upstream handoff semantics: image protocol rows are not
        // reusable after another renderer may have owned the terminal, so
        // retain their geometry as blank rows and force them to be rebuilt.
        self.core.previous_lines = state
            .previous_lines
            .into_iter()
            .map(|line| {
                if crate::terminal_image::is_image_line(&line) {
                    String::new()
                } else {
                    line
                }
            })
            .collect();
        self.core.max_lines_rendered = state.max_lines_rendered;
        self.core.previous_width = state.previous_width;
        self.core.previous_height = state.previous_height;
        self.core.last_cursor_position = None;
        self.core.last_cursor_visible = None;
    }
}

impl Component for TuiMainScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.core
            .root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(width)
    }
    fn handle_input(&mut self, key: &TuiKey) {
        self.core.dispatch_key(key);
    }
    fn handle_mouse(&mut self, event: &MouseEvent) {
        self.core.dispatch_mouse(event);
    }
}

/// Fullscreen/alternate-screen controller with retained viewport layout.
pub struct TuiAltScreen {
    core: ControllerCore,
    layout_root: Option<SharedComponent>,
    saved_capabilities: Option<crate::terminal_image::TerminalCapabilities>,
    search_overlay: Option<OverlayHandle>,
    search_component: Option<Arc<Mutex<AltScreenSearchComponent>>>,
    search_query: Option<Arc<Mutex<String>>>,
    search_matches: Vec<SearchMatch>,
    search_index: isize,
    selection_anchor: Option<SelectionPoint>,
    selection_focus: Option<SelectionPoint>,
    selected_text: Option<String>,
    scrollbar_drag: Option<ScrollbarDrag>,
}

/// State retained between a scrollbar press and the following drag/release
/// reports.  The layout frame is allowed to be replaced on every redraw, so
/// the shared scroll state—not a borrowed layout box—is the stable handle.
struct ScrollbarDrag {
    state: Arc<dyn crate::layout::ScrollLayoutState>,
    track_top: usize,
    max_thumb_top: usize,
    max_scroll_top: usize,
    grab_offset: usize,
}

impl TuiAltScreen {
    pub fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            core: ControllerCore::new(terminal),
            layout_root: None,
            saved_capabilities: None,
            search_overlay: None,
            search_component: None,
            search_query: None,
            search_matches: Vec::new(),
            search_index: -1,
            selection_anchor: None,
            selection_focus: None,
            selected_text: None,
            scrollbar_drag: None,
        }
    }

    pub fn mode(&self) -> TuiMode {
        TuiMode::Fullscreen
    }
    pub fn terminal(&self) -> Arc<Mutex<TerminalBackend>> {
        self.core.terminal.clone()
    }
    pub fn add_child(&mut self, component: SharedComponent) {
        self.core.add_child(component);
    }
    pub fn remove_child(&mut self, component: &SharedComponent) -> bool {
        self.core.remove_child(component)
    }
    pub fn clear(&mut self) {
        self.core.clear();
    }
    pub fn set_focus(&mut self, component: Option<SharedComponent>) {
        self.core.set_focus(component);
    }
    pub fn set_layout_root(&mut self, component: Option<SharedComponent>) {
        let unchanged = match (&self.layout_root, &component) {
            (None, None) => true,
            (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
            _ => false,
        };
        if unchanged {
            return;
        }
        self.layout_root = component;
        self.core.last_frame = None;
        self.core.request_render(false);
    }
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.core.show_hardware_cursor
    }
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        if self.core.show_hardware_cursor == enabled {
            return;
        }
        self.core.show_hardware_cursor = enabled;
        if !enabled {
            self.core
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .hide_cursor();
        }
        // Keep cursor toggles on the same coalesced owner-driven repaint path
        // as upstream requestRender(), avoiding synchronous layout work.
        self.core.request_render(false);
    }
    pub fn get_clear_on_shrink(&self) -> bool {
        self.core.clear_on_shrink
    }
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.core.clear_on_shrink = enabled;
    }
    pub fn full_redraws(&self) -> usize {
        self.core.full_redraws
    }
    pub fn has_overlay(&self) -> bool {
        self.core.overlays.has_visible_overlay()
    }
    pub fn show_overlay(
        &mut self,
        component: SharedComponent,
        options: OverlayOptions,
    ) -> OverlayHandle {
        let handle = self.core.overlays.show_overlay(component, options);
        self.core
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .hide_cursor();
        self.core.request_render(false);
        handle
    }
    pub fn hide_overlay(&mut self, handle: OverlayHandle) -> bool {
        let hidden = self.core.overlays.hide(handle);
        if hidden {
            if !self.core.overlays.has_visible_overlay() {
                self.core
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .hide_cursor();
            }
            self.core.request_render(false);
        }
        hidden
    }
    pub fn add_input_listener(
        &mut self,
        listener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> InputListenerHandle {
        self.core.add_input_listener(listener)
    }
    /// Install a host wake-up hook for timer-driven or asynchronous repaint
    /// requests. The hook must only wake the owner's event loop; rendering
    /// remains on that loop's thread.
    pub fn set_request_render_callback(&mut self, callback: Option<RequestRenderCallback>) {
        self.core.set_request_render_callback(callback);
    }
    /// Request a repaint without rendering synchronously. The returned force
    /// bit from [`Self::take_render_request`] lets an owner preserve upstream
    /// `requestRender(true)` semantics.
    pub fn request_render(&self, force: bool) {
        self.core.request_render(force);
    }
    /// Consume one coalesced repaint request, returning whether it requires a
    /// full render-state reset.
    pub fn take_render_request(&self) -> Option<bool> {
        self.core.take_render_request()
    }
    /// Check whether the controller has a repaint waiting for its owner.
    pub fn is_render_requested(&self) -> bool {
        self.core.is_render_requested()
    }
    /// Obtain the callback that can be passed to animated components such as
    /// `LoaderOptions` so their timer updates share this controller's wake-up
    /// and coalescing path.
    pub fn request_render_callback(&self) -> RequestRenderCallback {
        self.core.request_render_callback()
    }
    pub fn dispatch_raw(&mut self, raw: &str) {
        if self.core.listener_consumes(raw) {
            return;
        }
        if self.core.consume_cell_size_response(raw) {
            return;
        }
        if self.search_overlay.is_none() {
            if let Ok(Some(event)) = decode_mouse_event(raw) {
                if self.handle_viewport_mouse(event) {
                    if self.core.started {
                        self.core.request_render(false);
                    }
                    return;
                }
            }
        }
        let key = parse_key(raw);
        let release = is_key_release(raw);
        if !release && self.dispatch_viewport_key(&key) {
            if self.core.started {
                self.core.request_render(false);
            }
            return;
        }
        if release && self.matches_viewport_key(&key) {
            return;
        }
        self.core.dispatch_raw_after_listeners(raw);
        if self.core.started {
            self.core.request_render(false);
        }
    }

    /// Dispatch only input owned by the alternate-screen viewport.
    ///
    /// Interactive hosts that have their own application-level submit and
    /// editor routing need to let the viewport consume page navigation,
    /// search, mouse selection, and scrollbar gestures without also sending
    /// ordinary text to the focused editor. Returning `true` means the raw
    /// event was consumed by a viewport listener or the active search overlay.
    pub fn dispatch_viewport_input(&mut self, raw: &str) -> bool {
        if self.core.listener_consumes(raw) {
            return true;
        }
        if self.search_overlay.is_some() {
            let key = parse_key(raw);
            let bindings = get_keybindings();
            if !is_key_release(raw) && bindings.matches(&key, "tui.altScreen.searchNext") {
                self.navigate_search(1);
                return true;
            }
            if !is_key_release(raw) && bindings.matches(&key, "tui.altScreen.searchPrevious") {
                self.navigate_search(-1);
                return true;
            }
            if !is_key_release(raw) && bindings.matches(&key, "tui.altScreen.searchClose") {
                self.close_search();
                return true;
            }
            self.core.dispatch_raw_after_listeners(raw);
            self.core.request_render(false);
            return true;
        }
        if let Ok(Some(event)) = decode_mouse_event(raw) {
            if self.handle_viewport_mouse(event) {
                self.core.request_render(false);
                return true;
            }
        }
        let key = parse_key(raw);
        let consumed = if is_key_release(raw) {
            self.matches_viewport_key(&key)
        } else {
            self.dispatch_viewport_key(&key)
        };
        if consumed {
            self.core.request_render(false);
        }
        consumed
    }

    /// Match viewport-owned key releases without performing their action.
    /// Upstream `handleViewportInput` returns `consume: true` for both press
    /// and release events, preventing Kitty release reports from leaking into
    /// the editor while keeping navigation one-shot.
    fn matches_viewport_key(&self, key: &TuiKey) -> bool {
        let bindings = get_keybindings();
        if bindings.matches(key, "tui.altScreen.search") {
            return true;
        }
        if self.search_overlay.is_some()
            && (bindings.matches(key, "tui.altScreen.searchNext")
                || bindings.matches(key, "tui.altScreen.searchPrevious")
                || bindings.matches(key, "tui.altScreen.searchClose"))
        {
            return true;
        }
        if self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.as_ref())
            .is_none()
        {
            return false;
        }
        [
            "tui.altScreen.pageUp",
            "tui.altScreen.pageDown",
            "tui.altScreen.halfPageUp",
            "tui.altScreen.halfPageDown",
            "tui.altScreen.lineUp",
            "tui.altScreen.lineDown",
            "tui.altScreen.previousPrompt",
            "tui.altScreen.nextPrompt",
            "tui.altScreen.top",
            "tui.altScreen.bottom",
        ]
        .into_iter()
        .any(|binding| bindings.matches(key, binding))
    }

    fn primary_scroll_box(&self) -> Option<&crate::layout::LayoutBox> {
        self.core
            .last_frame
            .as_ref()
            .and_then(|frame| primary_scroll_box(&frame.root))
    }

    fn selection_point(&self, x: usize, y: usize) -> Option<SelectionPoint> {
        let box_ = self.primary_scroll_box()?;
        let state = box_.scroll_view.as_ref()?;
        let lines = box_.scroll_content_lines.as_ref()?;
        let row = y
            .saturating_sub(box_.rect.y)
            .min(box_.rect.height.saturating_sub(1));
        let column = x.saturating_sub(box_.rect.x).min(box_.rect.width);
        Some(SelectionPoint {
            row: state
                .scroll_top()
                .saturating_add(row)
                .min(lines.len().saturating_sub(1)),
            column,
        })
    }

    fn handle_viewport_mouse(&mut self, event: MouseEvent) -> bool {
        let state = self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.clone());

        // A scrollbar is an interactive part of the viewport, not transcript
        // text.  Preserve the pointer's position inside the thumb while
        // dragging so a grab at the top/bottom does not jump unexpectedly.
        if let Some(geometry) = self.primary_scroll_box().and_then(get_scrollbar_geometry) {
            if self.handle_scrollbar_mouse(event, geometry) {
                return true;
            }
        }

        match event.kind {
            MouseEventKind::WheelUp => {
                if let Some(state) = state {
                    state.scroll_by(-1);
                    return true;
                }
            }
            MouseEventKind::WheelDown => {
                if let Some(state) = state {
                    state.scroll_by(1);
                    return true;
                }
            }
            MouseEventKind::Press if event.button == MouseButton::Left => {
                self.selection_anchor = self.selection_point(event.x, event.y);
                self.selection_focus = self.selection_anchor;
                self.selected_text = None;
                return self.selection_anchor.is_some();
            }
            MouseEventKind::Drag | MouseEventKind::Motion
                if event.button == MouseButton::Left || self.selection_anchor.is_some() =>
            {
                if self.selection_anchor.is_some() {
                    self.selection_focus = self.selection_point(event.x, event.y);
                    return true;
                }
            }
            MouseEventKind::Release
                if matches!(event.button, MouseButton::Left | MouseButton::Other(3)) =>
            {
                if let Some(anchor) = self.selection_anchor {
                    self.selection_focus = self.selection_point(event.x, event.y);
                    let lines = self
                        .primary_scroll_box()
                        .and_then(|box_| box_.scroll_content_lines.clone());
                    if let (Some(focus), Some(lines)) = (self.selection_focus, lines) {
                        let text = extract_selection(
                            &lines,
                            SelectionRange {
                                start: anchor,
                                end: focus,
                            },
                        );
                        if !text.is_empty() {
                            self.copy_selection_to_terminal(&text);
                            self.selected_text = Some(text);
                        }
                    }
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn handle_scrollbar_mouse(&mut self, event: MouseEvent, geometry: ScrollbarGeometry) -> bool {
        let in_track = event.x == geometry.column
            && event.y >= geometry.track_top
            && event.y < geometry.track_top.saturating_add(geometry.track_height);
        let left_button = matches!(event.button, MouseButton::Left | MouseButton::Other(3));
        match event.kind {
            MouseEventKind::Press if event.button == MouseButton::Left && in_track => {
                let Some(state) = self
                    .core
                    .last_frame
                    .as_ref()
                    .and_then(|frame| frame.primary_scroll_view.clone())
                else {
                    return true;
                };
                let thumb_start = geometry.thumb_top;
                let thumb_end = thumb_start.saturating_add(geometry.thumb_height);
                let grab_offset = if event.y >= thumb_start && event.y < thumb_end {
                    event.y.saturating_sub(thumb_start)
                } else {
                    // Clicking the track pages toward the clicked location;
                    // using half a thumb gives the same intuitive result as
                    // beginning a drag from the centre of a newly positioned
                    // thumb.
                    geometry.thumb_height / 2
                };
                let max_thumb_top = geometry.track_height.saturating_sub(geometry.thumb_height);
                if event.y < thumb_start {
                    state.scroll_by(-(geometry.track_height.saturating_sub(1) as isize));
                } else if event.y >= thumb_end {
                    state.scroll_by(geometry.track_height.saturating_sub(1) as isize);
                }
                self.scrollbar_drag = Some(ScrollbarDrag {
                    state,
                    track_top: geometry.track_top,
                    max_thumb_top,
                    max_scroll_top: geometry.max_scroll_top,
                    grab_offset,
                });
                return true;
            }
            MouseEventKind::Drag | MouseEventKind::Motion if self.scrollbar_drag.is_some() => {
                let drag = self.scrollbar_drag.as_ref().expect("checked above");
                let desired = event
                    .y
                    .saturating_sub(drag.track_top)
                    .saturating_sub(drag.grab_offset)
                    .min(drag.max_thumb_top);
                let position = if drag.max_thumb_top == 0 {
                    0
                } else {
                    desired
                        .saturating_mul(drag.max_scroll_top)
                        .checked_div(drag.max_thumb_top)
                        .unwrap_or(0)
                };
                drag.state.scroll_to(position);
                return true;
            }
            MouseEventKind::Release if left_button && self.scrollbar_drag.is_some() => {
                self.scrollbar_drag = None;
                return true;
            }
            _ if in_track && left_button => return true,
            _ => {}
        }
        false
    }

    fn copy_selection_to_terminal(&self, text: &str) {
        let encoded = base64_encode(text.as_bytes());
        self.core
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .write_raw(&format!("\x1b]52;c;{encoded}\x07"));
    }

    pub fn selection(&self) -> Option<String> {
        self.selected_text.clone()
    }

    fn selection_range(&self) -> Option<SelectionRange> {
        let (start, end) = (self.selection_anchor?, self.selection_focus?);
        if start == end {
            return None;
        }
        if (start.row, start.column) <= (end.row, end.column) {
            Some(SelectionRange { start, end })
        } else {
            Some(SelectionRange {
                start: end,
                end: start,
            })
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selected_text = None;
        self.scrollbar_drag = None;
    }

    fn dispatch_viewport_key(&mut self, key: &TuiKey) -> bool {
        let bindings = get_keybindings();
        let matches = |id: &'static str| bindings.matches(key, id);
        if matches("tui.altScreen.search") {
            self.open_search();
            return true;
        }
        if self.search_overlay.is_some() && matches("tui.altScreen.searchClose") {
            self.close_search();
            return true;
        }
        if self.search_overlay.is_some() && matches("tui.altScreen.searchNext") {
            self.navigate_search(1);
            return true;
        }
        if self.search_overlay.is_some() && matches("tui.altScreen.searchPrevious") {
            self.navigate_search(-1);
            return true;
        }
        let Some(state) = self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.clone())
        else {
            return false;
        };
        let viewport = state.viewport_height().max(1);
        let step = |direction: isize| {
            state.scroll_by(direction);
        };
        if matches("tui.altScreen.pageUp") {
            step(-(viewport.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as isize));
        } else if matches("tui.altScreen.pageDown") {
            step(viewport.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as isize);
        } else if matches("tui.altScreen.halfPageUp") {
            step(-((viewport / 2).max(1) as isize));
        } else if matches("tui.altScreen.halfPageDown") {
            step((viewport / 2).max(1) as isize);
        } else if matches("tui.altScreen.lineUp") {
            step(-1);
        } else if matches("tui.altScreen.lineDown") {
            step(1);
        } else if matches("tui.altScreen.top") {
            state.scroll_to_start();
        } else if matches("tui.altScreen.bottom") {
            state.scroll_to_end();
        } else if matches("tui.altScreen.previousPrompt") {
            self.scroll_to_prompt(&state, -1);
        } else if matches("tui.altScreen.nextPrompt") {
            self.scroll_to_prompt(&state, 1);
        } else {
            return false;
        }
        true
    }

    fn open_search(&mut self) {
        if let Some(handle) = self.search_overlay {
            self.core.overlays.focus(handle);
            self.core.request_render(false);
            return;
        }
        let query = Arc::new(Mutex::new(String::new()));
        let query_for_callback = query.clone();
        let request_render = self.core.request_render_callback();
        let component = Arc::new(Mutex::new(
            AltScreenSearchComponent::new().with_query_callback(move |value| {
                *query_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value.to_string();
                request_render();
            }),
        ));
        let overlay_component: SharedComponent = component.clone();
        let handle = self.core.overlays.show_overlay(
            overlay_component,
            OverlayOptions {
                anchor: crate::tui::OverlayAnchor::TopRight,
                width: Some(crate::tui::SizeValue::Percent(40.0)),
                min_width: Some(24),
                margin: 1usize.into(),
                ..OverlayOptions::default()
            },
        );
        self.search_overlay = Some(handle);
        self.search_component = Some(component);
        self.search_query = Some(query);
        self.search_matches.clear();
        self.search_index = -1;
        self.core.request_render(false);
    }

    fn close_search(&mut self) {
        if let Some(handle) = self.search_overlay.take() {
            self.core.overlays.hide(handle);
        }
        self.search_component = None;
        self.search_query = None;
        self.search_matches.clear();
        self.search_index = -1;
        self.core.request_render(false);
    }

    fn navigate_search(&mut self, direction: isize) {
        self.refresh_search_state();
        if self.search_matches.is_empty() {
            return;
        }
        let count = self.search_matches.len() as isize;
        self.search_index = (self.search_index + direction).rem_euclid(count);
        self.reveal_search_match();
        self.core.request_render(false);
    }

    fn refresh_search_state(&mut self) {
        let Some((lines, state)) = self.core.last_frame.as_ref().map(|frame| {
            (
                primary_scroll_lines(&frame.root).map(ToOwned::to_owned),
                frame.primary_scroll_view.clone(),
            )
        }) else {
            return;
        };
        self.refresh_search_state_for_data(lines.as_deref(), state);
    }

    fn refresh_search_state_from_frame(&mut self, frame: &LayoutFrame) {
        let lines = primary_scroll_lines(&frame.root).map(ToOwned::to_owned);
        self.refresh_search_state_for_data(lines.as_deref(), frame.primary_scroll_view.clone());
    }

    fn refresh_search_state_for_data(
        &mut self,
        lines: Option<&[String]>,
        state: Option<Arc<dyn crate::layout::ScrollLayoutState>>,
    ) {
        let Some(query) = self.search_query.as_ref() else {
            return;
        };
        let query = query
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(lines) = lines else {
            return;
        };
        let matches = find_alt_screen_search_matches(lines, &query);
        if matches != self.search_matches {
            self.search_matches = matches;
            self.search_index = if self.search_matches.is_empty() {
                -1
            } else {
                0
            };
        }
        if let Some(component) = &self.search_component {
            component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_result(self.search_index, self.search_matches.len());
        }
        self.reveal_search_match_with_state(state);
    }

    fn reveal_search_match(&mut self) {
        let state = self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.clone());
        self.reveal_search_match_with_state(state);
    }

    fn reveal_search_match_with_state(
        &mut self,
        state: Option<Arc<dyn crate::layout::ScrollLayoutState>>,
    ) {
        let Some(state) = state else {
            return;
        };
        let Some(selected) = self.search_matches.get(self.search_index.max(0) as usize) else {
            return;
        };
        let Some(first) = selected.segments.first() else {
            return;
        };
        let viewport = state.viewport_height().max(1);
        let current = state.scroll_top();
        if first.row < current || first.row >= current + viewport {
            let target = first.row.saturating_sub(viewport / 3);
            state.scroll_to_with_options(target, true);
        }
    }

    fn scroll_to_prompt(
        &self,
        state: &Arc<dyn crate::layout::ScrollLayoutState>,
        direction: isize,
    ) {
        let Some(frame) = &self.core.last_frame else {
            return;
        };
        let Some(lines) = primary_scroll_lines(&frame.root) else {
            return;
        };
        let current = state.scroll_top() as isize;
        let mut row = current + direction;
        while row >= 0 && (row as usize) < lines.len() {
            if lines[row as usize].contains("\x1b]133;A") {
                state.scroll_by(row - current);
                return;
            }
            row += direction;
        }
    }
    pub fn dispatch_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::Key(raw) => self.dispatch_raw(&raw),
            TerminalEvent::Resize(width, height) => {
                self.core
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .set_size(width, height);
                // Resize is a scheduler event, not an inline render request;
                // the owner performs the coalesced frame on its render turn.
                self.core.request_render(false);
            }
        }
    }
    pub fn start(&mut self) -> io::Result<()> {
        if self.core.started {
            return Ok(());
        }
        self.suppress_iterm2_images();
        let result = self.core.start(TuiMode::Fullscreen);
        if let Err(error) = result {
            self.restore_saved_capabilities();
            return Err(error);
        }
        self.render_now(true);
        Ok(())
    }
    pub fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        if self.core.started {
            if self.search_overlay.is_some() {
                self.close_search();
            }
            self.clear_selection();
        }
        let document = if options.preserve_screen || !self.core.started {
            None
        } else {
            let width = self
                .core
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .width()
                .max(1);
            Some(
                self.layout_root
                    .clone()
                    .unwrap_or_else(|| self.core.root.clone())
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .render(width)
                    .into_iter()
                    .map(|line| {
                        let line = normalize_terminal_output(&line).replace(CURSOR_MARKER, "");
                        if visible_width(&line) > width {
                            slice_by_column(&line, 0, width)
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let result = self.core.stop(options);
        if result.is_ok() {
            if let Some(document) = document {
                let mut output = String::from(BEGIN_SYNC_UPDATE);
                output.push_str(DISABLE_AUTOWRAP);
                for (row, line) in document.iter().enumerate() {
                    if row > 0 {
                        output.push_str("\r\n");
                    }
                    output.push_str("\r\x1b[2K");
                    output.push_str(line);
                }
                output.push_str("\x1b[0m");
                output.push_str(ENABLE_AUTOWRAP);
                output.push_str("\r\n");
                output.push_str(SHOW_CURSOR);
                output.push_str(END_SYNC_UPDATE);
                self.core
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .write_raw(&output);
            }
        }
        self.restore_saved_capabilities();
        result
    }

    fn restore_saved_capabilities(&mut self) {
        if let Some(capabilities) = self.saved_capabilities.take() {
            crate::terminal_image::set_capabilities(capabilities);
        }
    }

    fn suppress_iterm2_images(&mut self) {
        if self.saved_capabilities.is_some() {
            return;
        }
        let capabilities = crate::terminal_image::get_capabilities();
        if capabilities.images != Some(crate::terminal_image::ImageProtocol::ITerm2) {
            return;
        }
        self.saved_capabilities = Some(capabilities);
        crate::terminal_image::set_capabilities(crate::terminal_image::TerminalCapabilities {
            images: None,
            ..capabilities
        });
        self.core.invalidate_components();
        if let Some(layout_root) = &self.layout_root {
            layout_root
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate();
        }
    }

    /// Temporarily leave raw and alternate-screen modes for SIGTSTP.
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.core.started {
            if self.search_overlay.is_some() {
                self.close_search();
            }
            self.clear_selection();
        }
        let result = self.core.suspend();
        if result.is_ok() {
            // Suspending leaves the alternate screen and must restore the
            // caller's image capability state until the screen is resumed.
            self.restore_saved_capabilities();
        }
        result
    }
    pub fn resume(&mut self) -> io::Result<()> {
        let was_suspended = self.core.is_suspended();
        let result = self.core.resume();
        if result.is_ok() && was_suspended {
            self.suppress_iterm2_images();
            self.render_now(true);
        }
        result
    }
    pub fn is_suspended(&self) -> bool {
        self.core.is_suspended()
    }
    pub fn render_now(&mut self, force: bool) {
        let root = self
            .layout_root
            .clone()
            .unwrap_or_else(|| self.core.root.clone());
        // Upstream refreshes search against the newly laid-out transcript
        // before composing the overlay. The core renderer owns terminal
        // output, so perform the lightweight layout pass here to avoid
        // exposing one frame with stale match counts or selection state.
        if self.search_overlay.is_some() {
            let (width, height) = {
                let terminal = self
                    .core
                    .terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                (terminal.width().max(1), terminal.height().max(1))
            };
            let search_frame = crate::layout::render_layout_frame(root.clone(), width, height);
            self.refresh_search_state_from_frame(&search_frame);
        }
        self.core.render_component_with_selection(
            root,
            TuiMode::Fullscreen,
            force,
            self.selection_range(),
        );
    }
    pub fn viewport_top(&self) -> usize {
        self.core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.as_ref())
            .map_or(0, |state| state.scroll_top())
    }
    pub fn is_following_output(&self) -> bool {
        self.core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.as_ref())
            .is_none_or(|state| state.is_following_end())
    }
    pub fn scroll_by(&mut self, lines: isize) -> isize {
        let Some(state) = self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.clone())
        else {
            return lines;
        };
        let remaining = state.scroll_by(lines);
        // Upstream scrollBy updates retained state and asks the owner to
        // repaint; it does not perform layout or terminal I/O inline.
        self.core.request_render(false);
        remaining
    }
    pub fn scroll_to_top(&mut self) {
        if let Some(state) = self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.clone())
        {
            state.scroll_to_start();
        }
        self.core.request_render(false);
    }
    pub fn scroll_to_bottom(&mut self) {
        if let Some(state) = self
            .core
            .last_frame
            .as_ref()
            .and_then(|frame| frame.primary_scroll_view.clone())
        {
            state.scroll_to_end();
        }
        self.core.request_render(false);
    }
}

impl Component for TuiAltScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.layout_root
            .clone()
            .unwrap_or_else(|| self.core.root.clone())
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(width)
    }
    fn handle_input(&mut self, key: &TuiKey) {
        if !self.dispatch_viewport_key(key) {
            self.core.dispatch_key(key);
        }
    }
    fn handle_mouse(&mut self, event: &MouseEvent) {
        if !self.handle_viewport_mouse(*event) {
            self.core.dispatch_mouse(event);
        }
    }
}

fn primary_scroll_lines(box_: &crate::layout::LayoutBox) -> Option<&[String]> {
    if box_
        .scroll_view
        .as_ref()
        .is_some_and(|state| state.primary())
    {
        if let Some(lines) = box_.scroll_content_lines.as_deref() {
            return Some(lines);
        }
    }
    box_.children.iter().find_map(primary_scroll_lines)
}

fn primary_scroll_box(box_: &crate::layout::LayoutBox) -> Option<&crate::layout::LayoutBox> {
    if box_
        .scroll_view
        .as_ref()
        .is_some_and(|state| state.primary())
    {
        return Some(box_);
    }
    box_.children.iter().find_map(primary_scroll_box)
}

/// Paint the active transcript selection after layout/overlay composition, as
/// upstream `TuiAltScreen.applySelection` does. Selection points are stored in
/// scroll-content coordinates, while `lines` contains viewport coordinates.
/// `visible_start` accounts for the fullscreen tail slice taken when an
/// overlay makes the composed frame taller than the terminal.
fn apply_selection_highlight(
    lines: &mut [String],
    root: &crate::layout::LayoutBox,
    frame_width: usize,
    selection: SelectionRange,
    visible_start: usize,
) {
    let Some(box_) = primary_scroll_box(root) else {
        return;
    };
    let Some(state) = box_.scroll_view.as_ref() else {
        return;
    };
    let Some(content_lines) = box_.scroll_content_lines.as_ref() else {
        return;
    };
    if content_lines.is_empty() {
        return;
    }

    let (start, end) = if (selection.start.row, selection.start.column)
        <= (selection.end.row, selection.end.column)
    {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    };
    let scroll_top = state.scroll_top();
    let min_row = box_.rect.y.max(box_.clip.y);
    let max_row = box_
        .rect
        .y
        .saturating_add(box_.rect.height)
        .min(box_.clip.y.saturating_add(box_.clip.height));
    let min_column = box_.rect.x.max(box_.clip.x);
    let max_column = frame_width
        .min(box_.rect.x.saturating_add(box_.rect.width))
        .min(box_.clip.x.saturating_add(box_.clip.width));
    if min_row >= max_row || min_column >= max_column {
        return;
    }

    for (screen_row, line) in lines.iter_mut().enumerate() {
        let layout_row = screen_row.saturating_add(visible_start);
        if layout_row < min_row || layout_row >= max_row || layout_row < box_.rect.y {
            continue;
        }
        let content_row = scroll_top.saturating_add(layout_row - box_.rect.y);
        if content_row < start.row || content_row > end.row {
            continue;
        }
        if crate::terminal_image::is_image_line(line) {
            continue;
        }
        let Some(content_line) = content_lines.get(content_row) else {
            continue;
        };
        let line_width = visible_width(line);
        let mut start_column = min_column;
        let mut end_column = max_column.min(line_width);
        if content_row == start.row {
            start_column =
                box_.rect
                    .x
                    .saturating_add(crate::components::alt_screen::snap_selection_column(
                        content_line,
                        start.column,
                        false,
                    ));
        }
        if content_row == end.row {
            end_column =
                box_.rect
                    .x
                    .saturating_add(crate::components::alt_screen::snap_selection_column(
                        content_line,
                        end.column,
                        true,
                    ));
        }
        start_column = start_column.max(min_column).min(line_width);
        end_column = end_column.min(max_column).min(line_width);
        if end_column <= start_column {
            continue;
        }

        let selected =
            slice_by_column_strict(line, start_column, end_column.saturating_sub(start_column));
        let segments = crate::utils::extract_segments(
            line,
            start_column,
            end_column,
            line_width.saturating_sub(end_column),
            true,
        );
        *line = format!(
            "{}{}{}",
            segments.before,
            highlight_selected_text(&selected),
            segments.after
        );
    }
}

/// Apply inverse video while preserving SGR changes inside the selected span.
/// Re-applying reverse video after each SGR reset is required for styled
/// transcript text; otherwise an embedded `\x1b[0m` silently ends the
/// selection halfway through the line.
fn highlight_selected_text(text: &str) -> String {
    let mut result = String::from("\x1b[7m");
    let mut pos = 0;
    while pos < text.len() {
        if let Some(code) = extract_ansi_code(text, pos) {
            let is_sgr = code.code.as_bytes().last() == Some(&b'm');
            result.push_str(&code.code);
            if is_sgr {
                result.push_str("\x1b[7m");
            }
            pos += code.length;
            continue;
        }
        let Some(ch) = text[pos..].chars().next() else {
            break;
        };
        result.push(ch);
        pos += ch.len_utf8();
    }
    result.push_str("\x1b[27m");
    result
}

/// Extract the hardware-cursor marker using the same viewport contract as
/// upstream `TuiBase.extractCursorPosition`: search the visible bottom
/// `height` rows from bottom to top, calculate the marker's visual column, and
/// remove the marker before the line reaches the terminal. Both regular and
/// fullscreen controllers use the marker; only their surrounding geometry and
/// terminal teardown differ.
fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let viewport_start = lines.len().saturating_sub(height);
    for row in (viewport_start..lines.len()).rev() {
        let Some(marker_index) = lines[row].find(CURSOR_MARKER) else {
            continue;
        };
        let column = visible_width(&lines[row][..marker_index]);
        lines[row].replace_range(marker_index..marker_index + CURSOR_MARKER.len(), "");
        return Some((row, column));
    }
    None
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as usize;
        let second = chunk.get(1).copied().unwrap_or(0) as usize;
        let third = chunk.get(2).copied().unwrap_or(0) as usize;
        output.push(TABLE[first >> 2] as char);
        output.push(TABLE[((first & 3) << 4) | (second >> 4)] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((second & 15) << 2) | (third >> 6)] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[third & 63] as char
        } else {
            '='
        });
    }
    output
}

/// Return Kitty image ids embedded in the control portion of a Kitty
/// graphics command.  Image payloads can contain arbitrary bytes, so only
/// the bytes before the first `;` are inspected.
fn kitty_image_ids_in_line(line: &str) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    let mut search_from = 0;
    while let Some(relative_start) = line[search_from..].find("\x1b_G") {
        let command_start = search_from + relative_start;
        let controls_start = command_start + "\x1b_G".len();
        let Some(relative_terminator) = line[controls_start..].find("\x1b\\") else {
            break;
        };
        let command_end = controls_start + relative_terminator;
        let controls_end = match line[controls_start..command_end].find(';') {
            Some(relative_separator) => controls_start + relative_separator,
            None => controls_start + relative_terminator,
        };
        for control in line[controls_start..controls_end].split(',') {
            let Some(value) = control.strip_prefix("i=") else {
                continue;
            };
            if let Ok(id) = value.parse::<u32>() {
                if id != 0 {
                    ids.insert(id);
                }
            }
        }
        search_from = controls_start + relative_terminator + "\x1b\\".len();
    }
    ids
}

fn kitty_image_ids(lines: &[String]) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    for line in lines {
        ids.extend(kitty_image_ids_in_line(line));
    }
    ids
}

/// Delete transmissions belonging to rows that are about to be replaced.
/// A full/clear redraw must delete every image from the retained frame before
/// clearing the screen because terminal screen clearing does not release
/// Kitty image data or placements.
fn kitty_cleanup_sequences(
    previous_lines: &[String],
    current_lines: &[String],
    full_redraw: bool,
) -> String {
    let mut ids = BTreeSet::new();
    if full_redraw {
        ids = kitty_image_ids(previous_lines);
    } else {
        for row in 0..previous_lines.len().max(current_lines.len()) {
            if previous_lines.get(row) != current_lines.get(row) {
                if let Some(previous) = previous_lines.get(row) {
                    ids.extend(kitty_image_ids_in_line(previous));
                }
            }
        }
    }
    ids.into_iter()
        .map(crate::terminal_image::delete_kitty_image)
        .collect()
}

fn clamp_controller_line(line: &str, width: usize) -> String {
    if crate::terminal_image::is_image_line(line) {
        line.to_string()
    } else {
        crate::layout::clamp_layout_line(line, width)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::components::image::{Image, ImageOptions, ImageTheme};
    use crate::components::loader::{LoaderIndicatorOptions, LoaderOptions};
    use crate::components::scroll_view::ScrollView;
    use crate::components::{Loader, Text};
    use crate::layout::{ScrollOverscroll, ScrollbarMode};
    use std::sync::mpsc;
    use std::time::Duration;

    fn text(value: &str) -> SharedComponent {
        Arc::new(Mutex::new(Text::new(value, 0, 0, None)))
    }

    #[derive(Default)]
    struct InputProbe {
        keys: Vec<TuiKey>,
    }

    impl Component for InputProbe {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["probe".into()]
        }

        fn handle_input(&mut self, key: &TuiKey) {
            self.keys.push(key.clone());
        }
    }

    struct CursorLines {
        lines: Vec<String>,
    }

    impl Component for CursorLines {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }
    }

    fn cursor_lines(lines: Vec<String>) -> SharedComponent {
        Arc::new(Mutex::new(CursorLines { lines }))
    }

    struct EchoProbe {
        value: String,
    }

    impl Component for EchoProbe {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![self.value.clone()]
        }

        fn handle_input(&mut self, key: &TuiKey) {
            self.value.push_str(&key.base);
        }
    }

    fn take_capture(terminal: &Arc<Mutex<TerminalBackend>>) -> String {
        String::from_utf8(
            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take_output_capture(),
        )
        .unwrap()
    }

    fn terminal_capability_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn kitty_cleanup_deletes_changed_and_full_frame_image_ids() {
        let previous = vec![
            "\x1b_Ga=T,i=7;first-payload\x1b\\".to_string(),
            "unchanged".to_string(),
            "\x1b_Ga=T,i=11;second-payload\x1b\\".to_string(),
        ];
        let current = vec!["replacement".to_string(), "unchanged".to_string()];

        assert_eq!(
            kitty_cleanup_sequences(&previous, &current, false),
            format!(
                "{}{}",
                crate::terminal_image::delete_kitty_image(7),
                crate::terminal_image::delete_kitty_image(11)
            )
        );
        assert_eq!(
            kitty_cleanup_sequences(&previous, &current, true),
            format!(
                "{}{}",
                crate::terminal_image::delete_kitty_image(7),
                crate::terminal_image::delete_kitty_image(11)
            )
        );
        assert!(kitty_cleanup_sequences(&previous, &previous, false).is_empty());
    }

    #[test]
    fn kitty_image_id_parser_ignores_payload_and_malformed_commands() {
        let line = "prefix\x1b_Ga=T,i=19;payload contains \x1b_Ga=T,i=99;fake\x1b\\ suffix\x1b_Ga=p,q=2,C=1,i=23\x1b\\";
        assert_eq!(
            kitty_image_ids_in_line(line)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![19, 23]
        );
        assert!(kitty_image_ids_in_line("\x1b_Ga=T,i=;payload\x1b\\").is_empty());
    }

    #[test]
    fn selection_highlight_reapplies_inverse_after_sgr_reset() {
        assert_eq!(
            highlight_selected_text("al\x1b[0mpha"),
            "\x1b[7mal\x1b[0m\x1b[7mpha\x1b[27m"
        );
    }

    #[test]
    fn kitty_screen_reuses_uploaded_transmissions_as_placements() {
        let image_id = 4801;
        crate::terminal_image::register_kitty_image_metadata(
            crate::terminal_image::KittyImageMetadata {
                image_id,
                columns: 2,
                rows: 1,
                width_px: 20,
                height_px: 10,
            },
        );
        let transmission = crate::terminal_image::encode_kitty("AAAA", 2, 1, Some(image_id), false);
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut core = ControllerCore::new(terminal);

        let first = core.prepare_kitty_screen(std::slice::from_ref(&transmission));
        assert_eq!(first.lines, vec![transmission.clone()]);
        assert!(first.stale_image_deletion.is_empty());
        assert!(first.evicted_image_deletion.is_empty());

        let second = core.prepare_kitty_screen(std::slice::from_ref(&transmission));
        assert!(second.lines[0].contains("\x1b_Ga=p,q=2"));
        assert!(!second.lines[0].contains("AAAA"));
        assert!(second.stale_image_deletion.is_empty());
        assert_eq!(core.uploaded_kitty_images.len(), 1);

        crate::terminal_image::register_kitty_image_metadata(
            crate::terminal_image::KittyImageMetadata {
                image_id,
                columns: 2,
                rows: 1,
                width_px: 30,
                height_px: 10,
            },
        );
        let replacement = crate::terminal_image::encode_kitty("BBBB", 2, 1, Some(image_id), false);
        let third = core.prepare_kitty_screen(std::slice::from_ref(&replacement));
        assert_eq!(third.lines, vec![replacement]);
        assert!(third
            .stale_image_deletion
            .contains("\x1b_Ga=d,d=I,i=4801,q=2\x1b\\"));
    }

    #[test]
    fn kitty_screen_evicts_oldest_offscreen_upload_over_image_quota() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut core = ControllerCore::new(terminal);
        core.uploaded_kitty_images = (0..=MAX_CACHED_OFFSCREEN_KITTY_IMAGES)
            .map(|index| {
                (
                    4900 + index as u32,
                    CachedKittyImage {
                        transmission_generation: index as u64,
                        transmission_bytes: 1,
                        estimated_decoded_bytes: 1,
                    },
                )
            })
            .collect();

        let prepared = core.prepare_kitty_screen(&[]);
        assert!(prepared.evicted_image_deletion.contains("i=4900,q=2"));
        assert_eq!(
            core.uploaded_kitty_images.len(),
            MAX_CACHED_OFFSCREEN_KITTY_IMAGES
        );
    }

    #[test]
    fn image_rows_clear_iterm_placements_before_redraw() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 2)));
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        let child = Arc::new(Mutex::new(CursorLines {
            lines: vec!["\x1b]1337;File=inline=1;width=2;height=auto:AAAA\x07".to_string()],
        }));
        let mut core = ControllerCore::new(terminal.clone());
        let child_component: SharedComponent = child.clone();
        core.add_child(child_component);
        core.render_component(core.root.clone(), TuiMode::Fullscreen, true);
        let _ = take_capture(&terminal);

        child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lines = vec!["replacement".to_string()];
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        core.render_component(core.root.clone(), TuiMode::Fullscreen, false);
        let output = take_capture(&terminal);
        assert!(output.contains("\x1b[2J"));
        assert!(output.contains("replacement"));
    }

    #[test]
    fn controller_consumes_cell_size_responses_before_key_dispatch() {
        let _capability_guard = terminal_capability_lock();
        let original = crate::terminal_image::get_cell_dimensions();
        crate::terminal_image::set_cell_dimensions(11, 22);
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal);

        tui.dispatch_raw("\x1b[6;0;9t");
        assert!(!tui.is_render_requested());
        assert_eq!(crate::terminal_image::get_cell_dimensions(), (11, 22));

        tui.dispatch_raw("\x1b[6;18;9t");
        assert_eq!(crate::terminal_image::get_cell_dimensions(), (9, 18));
        assert_eq!(tui.take_render_request(), Some(false));

        tui.dispatch_raw("\x1b[6;18;9;1t");
        assert_eq!(tui.take_render_request(), None);
        crate::terminal_image::set_cell_dimensions(original.0, original.1);
    }

    #[test]
    fn image_capability_queries_cell_size_when_controller_starts() {
        let _capability_guard = terminal_capability_lock();
        let original = crate::terminal_image::get_capabilities();
        crate::terminal_image::set_capabilities(crate::terminal_image::TerminalCapabilities {
            images: Some(crate::terminal_image::ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        });

        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.start().unwrap();
        let output = take_capture(&terminal);
        let stop_result = tui.stop(TuiStopOptions::default());
        crate::terminal_image::set_capabilities(original);

        stop_result.unwrap();
        assert!(
            output.contains("\x1b[16t"),
            "missing cell-size query: {output:?}"
        );
    }

    #[test]
    fn fullscreen_iterm2_images_fallback_and_restore_across_restart() {
        let _capability_guard = terminal_capability_lock();
        let original_capabilities = crate::terminal_image::get_capabilities();
        let iterm_capabilities = crate::terminal_image::TerminalCapabilities {
            images: Some(crate::terminal_image::ImageProtocol::ITerm2),
            true_color: true,
            hyperlinks: true,
        };
        crate::terminal_image::set_capabilities(iterm_capabilities);

        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.add_child(Arc::new(Mutex::new(Image::new(
            "AAAA",
            "image/png",
            ImageTheme {
                fallback_color: Box::new(|value: &str| value.to_owned()),
            },
            ImageOptions::default(),
        ))));

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.start().unwrap();
        let first_output = take_capture(&terminal);
        let first_active_capabilities = crate::terminal_image::get_capabilities();
        tui.stop(TuiStopOptions {
            preserve_screen: true,
        })
        .unwrap();
        let first_restored_capabilities = crate::terminal_image::get_capabilities();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.start().unwrap();
        let second_output = take_capture(&terminal);
        let second_active_capabilities = crate::terminal_image::get_capabilities();
        tui.stop(TuiStopOptions {
            preserve_screen: true,
        })
        .unwrap();
        let second_restored_capabilities = crate::terminal_image::get_capabilities();
        crate::terminal_image::set_capabilities(original_capabilities);

        for output in [first_output, second_output] {
            assert!(
                output.contains("[Image:"),
                "missing text image fallback: {output:?}"
            );
            assert!(
                !output.contains("\x1b]1337;File="),
                "iTerm2 image escaped alt screen: {output:?}"
            );
        }
        assert_eq!(first_active_capabilities.images, None);
        assert_eq!(second_active_capabilities.images, None);
        assert_eq!(first_restored_capabilities, iterm_capabilities);
        assert_eq!(second_restored_capabilities, iterm_capabilities);
    }

    #[test]
    fn fullscreen_iterm2_images_fallback_survives_suspend_resume() {
        let _capability_guard = terminal_capability_lock();
        let original_capabilities = crate::terminal_image::get_capabilities();
        let iterm_capabilities = crate::terminal_image::TerminalCapabilities {
            images: Some(crate::terminal_image::ImageProtocol::ITerm2),
            true_color: true,
            hyperlinks: true,
        };
        crate::terminal_image::set_capabilities(iterm_capabilities);

        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal);
        tui.start().unwrap();
        assert_eq!(crate::terminal_image::get_capabilities().images, None);

        tui.suspend().unwrap();
        assert!(tui.is_suspended());
        assert_eq!(
            crate::terminal_image::get_capabilities(),
            iterm_capabilities
        );

        tui.resume().unwrap();
        assert!(!tui.is_suspended());
        assert_eq!(crate::terminal_image::get_capabilities().images, None);

        tui.stop(TuiStopOptions {
            preserve_screen: true,
        })
        .unwrap();
        assert_eq!(
            crate::terminal_image::get_capabilities(),
            iterm_capabilities
        );
        crate::terminal_image::set_capabilities(original_capabilities);
    }

    #[test]
    fn image_lines_are_not_truncated_as_visible_text() {
        let image_line = "prefix\x1b_Ga=T,i=31;AAAA\x1b\\suffix";
        assert_eq!(clamp_controller_line(image_line, 1), image_line);
        assert_eq!(clamp_controller_line("abcdef", 3), "abc");
    }

    #[test]
    fn repaint_requests_are_coalesced_and_preserve_force_semantics() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal);
        let notifications = Arc::new(Mutex::new(0usize));
        let notifications_for_callback = notifications.clone();
        tui.set_request_render_callback(Some(Arc::new(move || {
            *notifications_for_callback
                .lock()
                .unwrap_or_else(|error| error.into_inner()) += 1;
        })));

        tui.request_render(false);
        tui.request_render(false);
        assert_eq!(
            *notifications
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            1
        );
        assert!(tui.is_render_requested());
        assert_eq!(tui.take_render_request(), Some(false));
        assert!(!tui.is_render_requested());
        assert_eq!(tui.take_render_request(), None);

        tui.request_render(false);
        tui.request_render(true);
        assert_eq!(
            *notifications
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            3
        );
        assert_eq!(tui.take_render_request(), Some(true));
        assert_eq!(tui.take_render_request(), None);
    }

    #[test]
    fn replacing_layout_root_invalidates_cached_frame_and_requests_repaint() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal);
        let notifications = Arc::new(Mutex::new(0usize));
        let notifications_for_callback = notifications.clone();
        tui.set_request_render_callback(Some(Arc::new(move || {
            *notifications_for_callback
                .lock()
                .unwrap_or_else(|error| error.into_inner()) += 1;
        })));

        let first = text("first");
        tui.set_layout_root(Some(first));
        assert_eq!(tui.take_render_request(), Some(false));
        tui.render_now(true);
        assert!(tui.core.last_frame.is_some());

        let second = text("second");
        tui.set_layout_root(Some(second.clone()));
        assert!(tui.core.last_frame.is_none());
        assert!(tui.is_render_requested());
        assert_eq!(tui.take_render_request(), Some(false));
        assert_eq!(
            *notifications
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            2
        );

        tui.set_layout_root(Some(second));
        assert_eq!(tui.take_render_request(), None);
    }

    #[test]
    fn public_scroll_actions_queue_repaint_without_inline_terminal_io() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let content = text(
            &(1..=12)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let view = Arc::new(Mutex::new(ScrollView::with_options(
            content,
            true,
            ScrollOverscroll::Chain,
        )));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.set_layout_root(Some(view.clone()));
        tui.render_now(true);
        assert!(!tui.is_render_requested());

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        assert_eq!(tui.scroll_by(-1), 0);
        assert!(tui.is_render_requested());
        assert!(take_capture(&terminal).is_empty());
        assert_eq!(tui.viewport_top(), 7);
        let _ = tui.take_render_request();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.scroll_to_bottom();
        assert!(tui.is_render_requested());
        assert!(take_capture(&terminal).is_empty());
        assert_eq!(tui.viewport_top(), 8);
        let _ = tui.take_render_request();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.scroll_to_top();
        assert!(tui.is_render_requested());
        assert!(take_capture(&terminal).is_empty());
        assert_eq!(tui.viewport_top(), 0);
    }

    #[test]
    fn fullscreen_layout_wires_scrollbar_auto_hide_to_controller_repaint() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(40, 4)));
        let mut tui = TuiAltScreen::new(terminal);
        let (sender, receiver) = mpsc::channel();
        tui.set_request_render_callback(Some(Arc::new(move || {
            let _ = sender.send(());
        })));

        let content = (0..12)
            .map(|index| format!("transcript line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = Arc::new(Mutex::new(ScrollView::with_options(
            text(&content),
            true,
            ScrollOverscroll::Chain,
        )));
        {
            let mut view_guard = view.lock().unwrap_or_else(|error| error.into_inner());
            view_guard.set_height(3);
            view_guard.set_scrollbar(ScrollbarMode::Auto);
            view_guard.set_scrollbar_hide_delay(Duration::from_millis(20));
            view_guard.set_scrollbar_active(false);
        }
        tui.set_layout_root(Some(view.clone()));
        tui.render_now(true);
        // set_layout_root wakes the owner before the initial frame. The
        // render consumes that request state, but the callback event remains
        // queued in this test channel; discard it before observing the
        // timer-driven auto-hide wake below.
        while receiver.try_recv().is_ok() {}
        assert!(
            view.lock()
                .unwrap_or_else(|error| error.into_inner())
                .content_height()
                > 3
        );

        // Re-arm activity after layout has published content metrics. The
        // timer callback must wake the owner without rendering off-thread.
        {
            let mut view_guard = view.lock().unwrap_or_else(|error| error.into_inner());
            view_guard.set_scrollbar_active(true);
            view_guard.set_scrollbar_active(false);
        }
        assert!(receiver.recv_timeout(Duration::from_millis(250)).is_ok());
        assert!(tui.is_render_requested());
        assert_eq!(tui.take_render_request(), Some(false));
    }

    #[test]
    fn animated_loader_can_share_controller_repaint_callback() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let tui = TuiAltScreen::new(terminal);
        let (sender, receiver) = mpsc::channel();
        let controller_callback = tui.request_render_callback();
        let mut loader = Loader::with_options(
            "working",
            LoaderOptions::default()
                .with_indicator(Some(
                    LoaderIndicatorOptions::default()
                        .with_frames(["a", "b"])
                        .with_interval_ms(10.0),
                ))
                .with_request_render(move || {
                    let _ = sender.send(());
                    controller_callback();
                }),
        );

        // Construction refreshes once; consume that request so the following
        // notification proves the interval worker crossed the same seam.
        let _ = tui.take_render_request();
        while receiver.try_recv().is_ok() {}
        let timer_notified = receiver.recv_timeout(Duration::from_millis(250)).is_ok();
        loader.stop();

        assert!(timer_notified);
        assert!(tui.is_render_requested());
        assert_eq!(tui.take_render_request(), Some(false));
    }

    #[test]
    fn main_and_alt_dispatch_raw_call_listeners_once_before_delivery() {
        let main_terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut main = TuiMainScreen::new(main_terminal);
        let main_probe = Arc::new(Mutex::new(InputProbe::default()));
        let main_component: SharedComponent = main_probe.clone();
        main.add_child(main_component.clone());
        main.set_focus(Some(main_component));
        let main_calls = Arc::new(Mutex::new(Vec::new()));
        let main_calls_for_listener = main_calls.clone();
        let _main_listener = main.add_input_listener(move |raw| {
            main_calls_for_listener
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(raw.to_string());
            raw == "consume"
        });

        main.dispatch_raw("consume");
        main.dispatch_raw("pass");
        assert_eq!(
            *main_calls.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["consume", "pass"]
        );
        assert_eq!(
            main_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys
                .len(),
            1
        );
        assert_eq!(
            main_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys[0]
                .base,
            "pass"
        );

        let alt_terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut alt = TuiAltScreen::new(alt_terminal);
        let alt_probe = Arc::new(Mutex::new(InputProbe::default()));
        let alt_component: SharedComponent = alt_probe.clone();
        alt.add_child(alt_component.clone());
        alt.set_focus(Some(alt_component));
        let alt_calls = Arc::new(Mutex::new(Vec::new()));
        let alt_calls_for_listener = alt_calls.clone();
        let _alt_listener = alt.add_input_listener(move |raw| {
            alt_calls_for_listener
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(raw.to_string());
            raw == "consume"
        });

        alt.dispatch_raw("consume");
        alt.dispatch_raw("pass");
        assert_eq!(
            *alt_calls.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["consume", "pass"]
        );
        assert_eq!(
            alt_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys
                .len(),
            1
        );
        assert_eq!(
            alt_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys[0]
                .base,
            "pass"
        );
    }

    #[test]
    fn raw_kitty_release_is_not_delivered_to_focused_component() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal);
        let probe = Arc::new(Mutex::new(InputProbe::default()));
        let component: SharedComponent = probe.clone();
        tui.add_child(component.clone());
        tui.set_focus(Some(component));

        tui.dispatch_raw("\x1b[97u");
        tui.dispatch_raw("\x1b[97;1:3u");

        let probe = probe.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(probe.keys.len(), 1);
        assert_eq!(probe.keys[0].base, "a");
    }

    #[test]
    fn viewport_consumes_search_release_without_repeating_open_action() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal);

        // Kitty flag-2 release for Ctrl+Shift+F. Upstream consumes this
        // release even when no search overlay exists, but does not open one.
        assert!(tui.dispatch_viewport_input("\x1b[102;6:3u"));
        assert!(!tui.has_overlay());
    }

    #[test]
    fn fullscreen_extracts_cursor_marker_uses_visible_column_and_shows_cursor() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.set_show_hardware_cursor(true);
        tui.add_child(cursor_lines(vec![
            "header".into(),
            format!("\x1b[31m界\x1b[0m{CURSOR_MARKER}tail"),
        ]));

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(true);
        let output = take_capture(&terminal);

        assert!(!output.contains(CURSOR_MARKER));
        assert!(output.contains("\x1b[2;1H\x1b[2K\x1b[31m界\x1b[0mtail"));
        assert!(output.contains("\x1b[2;3H"));
        assert!(output.contains(SHOW_CURSOR));
        assert!(!output.contains(HIDE_CURSOR));
    }

    #[test]
    fn fullscreen_hides_cursor_when_no_visible_marker_exists() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.set_show_hardware_cursor(false);
        tui.add_child(cursor_lines(vec!["no marker".into()]));

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(true);
        let output = take_capture(&terminal);

        assert!(!output.contains(CURSOR_MARKER));
        assert!(output.contains(HIDE_CURSOR));
        assert!(!output.contains(SHOW_CURSOR));
        assert!(!output.contains("\x1b[1;1H\x1b[?25l"));
    }

    #[test]
    fn fullscreen_hardware_cursor_toggle_hides_immediately_like_upstream() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.set_show_hardware_cursor(true);

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.set_show_hardware_cursor(false);
        let output = take_capture(&terminal);

        assert_eq!(output, HIDE_CURSOR);
        assert!(!tui.get_show_hardware_cursor());
    }

    #[test]
    fn unchanged_frames_are_not_written_but_cursor_only_changes_are() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.set_show_hardware_cursor(true);
        let lines_state = Arc::new(Mutex::new(CursorLines {
            lines: vec![format!("ab{CURSOR_MARKER}cd")],
        }));
        let lines: SharedComponent = lines_state.clone();
        tui.add_child(lines.clone());

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(true);
        let _ = take_capture(&terminal);

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        assert!(take_capture(&terminal).is_empty());

        lines_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lines = vec![format!("abc{CURSOR_MARKER}d")];
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        let output = take_capture(&terminal);
        assert!(!output.contains(BEGIN_SYNC_UPDATE));
        assert!(output.contains("\x1b[1;4H"));
        assert!(output.contains(SHOW_CURSOR));
    }

    #[test]
    fn started_controller_dispatch_queues_repaint_until_owner_renders() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        let probe = Arc::new(Mutex::new(EchoProbe { value: "a".into() }));
        tui.add_child(probe.clone());
        tui.set_focus(Some(probe));
        tui.start().unwrap();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.dispatch_raw("b");
        assert!(tui.is_render_requested());
        assert!(take_capture(&terminal).is_empty());

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        let output = take_capture(&terminal);
        assert!(output.contains("ab"));

        tui.stop(TuiStopOptions::default()).unwrap();
    }

    #[test]
    fn started_controller_coalesces_rapid_input_before_one_owner_frame() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        let probe = Arc::new(Mutex::new(EchoProbe {
            value: String::new(),
        }));
        tui.add_child(probe.clone());
        tui.set_focus(Some(probe.clone()));
        let wakeups = Arc::new(Mutex::new(0usize));
        let wakeups_for_callback = wakeups.clone();
        tui.set_request_render_callback(Some(Arc::new(move || {
            *wakeups_for_callback
                .lock()
                .unwrap_or_else(|error| error.into_inner()) += 1;
        })));
        tui.start().unwrap();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        for key in ["a", "b", "c", "d"] {
            tui.dispatch_raw(key);
        }

        assert_eq!(
            probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .value,
            "abcd"
        );
        assert_eq!(
            *wakeups.lock().unwrap_or_else(|error| error.into_inner()),
            1
        );
        assert!(tui.is_render_requested());
        assert!(take_capture(&terminal).is_empty());

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        let output = take_capture(&terminal);
        assert!(output.contains("abcd"));
        assert!(!tui.is_render_requested());

        tui.stop(TuiStopOptions::default()).unwrap();
    }

    #[test]
    fn regular_renderer_positions_hardware_cursor_from_marker() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.set_show_hardware_cursor(true);
        tui.add_child(cursor_lines(vec![format!("ab{CURSOR_MARKER}cd")]));

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(true);
        let output = take_capture(&terminal);

        assert!(!output.contains(CURSOR_MARKER));
        assert!(output.contains("ab\x1b[0mcd") || output.contains("abcd"));
        assert!(output.contains(SHOW_CURSOR));
        assert!(output.contains("\x1b[1;3H"));
    }

    #[test]
    fn regular_first_frame_writes_natural_lines_to_scrollback() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.add_child(text("one\ntwo"));

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        let output = take_capture(&terminal);

        assert!(output.contains("one                 \r\ntwo                 "));
        assert!(!output.contains("\x1b[2;1H"));
    }

    #[test]
    fn regular_hardware_cursor_disable_hides_immediately() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.set_show_hardware_cursor(true);
        assert!(tui.get_show_hardware_cursor());

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.set_show_hardware_cursor(false);
        let output = take_capture(&terminal);

        assert_eq!(output, HIDE_CURSOR);
        assert!(!tui.get_show_hardware_cursor());
    }

    #[test]
    fn regular_stop_leaves_scrollback_instead_of_clearing_screen() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.add_child(text("scrollback"));
        tui.start().unwrap();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.stop(TuiStopOptions::default()).unwrap();
        let output = take_capture(&terminal);

        assert!(output.contains(" \r\n"));
        assert!(!output.contains(CLEAR_SCREEN_HOME));
        tui.stop(TuiStopOptions::default()).unwrap();
    }

    #[test]
    fn restoring_main_screen_state_blanks_image_rows_for_handoff() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiMainScreen::new(terminal);
        tui.restore_render_state(TuiMainScreenRenderState {
            previous_lines: vec![
                "\x1b_Ga=T,i=7;payload\x1b\\".to_string(),
                "retained text".to_string(),
            ],
            previous_width: 20,
            previous_height: 4,
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 9,
            previous_viewport_top: 0,
        });

        let restored = tui.capture_render_state();
        assert_eq!(restored.previous_lines, vec!["", "retained text"]);
        assert_eq!(restored.max_lines_rendered, 9);
    }

    #[test]
    fn regular_clear_on_shrink_uses_rendered_high_water_mark() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let lines = Arc::new(Mutex::new(CursorLines {
            lines: vec!["one".into(), "two".into(), "three".into(), "four".into()],
        }));
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.set_clear_on_shrink(true);
        tui.add_child(lines.clone());
        tui.render_now(true);
        assert_eq!(tui.capture_render_state().max_lines_rendered, 4);

        lines
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lines
            .truncate(1);
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        let output = take_capture(&terminal);

        assert!(output.contains(CLEAR_SCREEN_HOME));
        assert_eq!(tui.capture_render_state().max_lines_rendered, 1);
    }

    #[test]
    fn regular_resize_clears_scrollback_before_full_repaint() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let lines = Arc::new(Mutex::new(CursorLines {
            lines: vec!["before resize".into()],
        }));
        let mut tui = TuiMainScreen::new(terminal.clone());
        tui.add_child(lines);
        tui.render_now(false);

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_size(10, 3);
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.render_now(false);
        let output = take_capture(&terminal);

        assert!(output.contains("\x1b[2J\x1b[H\x1b[3J"));
        assert!(output.contains("before res"));
    }

    #[test]
    fn fullscreen_cursor_extraction_scans_visible_rows_from_bottom() {
        let mut lines = vec![
            format!("top{CURSOR_MARKER}"),
            "hidden".into(),
            "visible".into(),
            format!("bottom{CURSOR_MARKER}tail"),
        ];

        assert_eq!(extract_cursor_position(&mut lines, 2), Some((3, 6)));
        assert!(lines[0].contains(CURSOR_MARKER));
        assert!(!lines[3].contains(CURSOR_MARKER));
        assert_eq!(lines[3], "bottomtail");
    }

    #[test]
    fn main_and_alt_controllers_render_resize_and_cleanup_idempotently() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut main = TuiMainScreen::new(terminal.clone());
        main.add_child(text("main"));
        main.render_now(true);
        assert_eq!(main.full_redraws(), 1);
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_size(8, 2);
        main.render_now(false);
        assert_eq!(main.capture_render_state().previous_width, 8);
        main.stop(TuiStopOptions::default()).unwrap();

        let mut alt = TuiAltScreen::new(terminal);
        alt.add_child(text("alt"));
        alt.render_now(true);
        alt.stop(TuiStopOptions {
            preserve_screen: true,
        })
        .unwrap();
        alt.stop(TuiStopOptions::default()).unwrap();
    }

    #[test]
    fn fullscreen_stop_restores_document_after_leaving_alt_screen() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 3)));
        let mut tui = TuiAltScreen::new(terminal.clone());
        tui.add_child(text("first\nsecond\nthird\nfourth"));
        tui.start().unwrap();

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        tui.stop(TuiStopOptions::default()).unwrap();
        let output = take_capture(&terminal);
        let exit_index = output
            .find(crate::terminal::EXIT_ALT_SCREEN)
            .expect("fullscreen stop exits the alternate screen");
        let document_index = output
            .find("first")
            .expect("fullscreen stop restores the document");
        assert!(document_index > exit_index);
        assert!(output.contains("\r\x1b[2Kfirst"));
        assert!(output.contains("\r\n\r\x1b[2Ksecond"));
    }

    #[test]
    fn nested_overlay_focus_and_mouse_dispatch_follow_lifecycle() {
        struct Probe {
            keys: usize,
            mice: usize,
            focused: bool,
        }
        impl Component for Probe {
            fn render(&self, _width: usize) -> Vec<String> {
                vec!["probe".into()]
            }
            fn handle_input(&mut self, _key: &TuiKey) {
                self.keys += 1;
            }
            fn handle_mouse(&mut self, _event: &MouseEvent) {
                self.mice += 1;
            }
            fn set_focused(&mut self, focused: bool) {
                self.focused = focused;
            }
        }
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut tui = TuiAltScreen::new(terminal);
        let base = Arc::new(Mutex::new(Probe {
            keys: 0,
            mice: 0,
            focused: false,
        }));
        let modal = Arc::new(Mutex::new(Probe {
            keys: 0,
            mice: 0,
            focused: false,
        }));
        tui.add_child(base.clone());
        tui.set_focus(Some(base));
        let handle = tui.show_overlay(modal.clone(), OverlayOptions::default());
        tui.dispatch_raw("x");
        tui.dispatch_raw("\x1b[<0;2;2M");
        assert_eq!(
            modal.lock().unwrap_or_else(|error| error.into_inner()).keys,
            1
        );
        assert_eq!(
            modal.lock().unwrap_or_else(|error| error.into_inner()).mice,
            1
        );
        assert!(tui.hide_overlay(handle));
        assert!(!tui.has_overlay());
    }

    macro_rules! assert_overlay_repaint_contract {
        ($constructor:path) => {{
            let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
            let mut tui = $constructor(terminal.clone());
            let wakeups = Arc::new(Mutex::new(0usize));
            let wakeups_for_callback = wakeups.clone();
            tui.set_request_render_callback(Some(Arc::new(move || {
                *wakeups_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) += 1;
            })));

            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .begin_output_capture();
            let handle = tui.show_overlay(text("modal"), OverlayOptions::default());
            assert_eq!(take_capture(&terminal), HIDE_CURSOR);
            assert_eq!(
                *wakeups.lock().unwrap_or_else(|error| error.into_inner()),
                1
            );
            assert_eq!(tui.take_render_request(), Some(false));

            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .begin_output_capture();
            assert!(tui.hide_overlay(handle));
            assert_eq!(take_capture(&terminal), HIDE_CURSOR);
            assert_eq!(
                *wakeups.lock().unwrap_or_else(|error| error.into_inner()),
                2
            );
            assert_eq!(tui.take_render_request(), Some(false));
        }};
    }

    #[test]
    fn overlays_hide_cursor_and_queue_repaints_in_both_modes() {
        assert_overlay_repaint_contract!(TuiMainScreen::new);
        assert_overlay_repaint_contract!(TuiAltScreen::new);
    }

    macro_rules! assert_cursor_repaint_contract {
        ($constructor:path) => {{
            let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
            let mut tui = $constructor(terminal.clone());
            let wakeups = Arc::new(Mutex::new(0usize));
            let wakeups_for_callback = wakeups.clone();
            tui.set_request_render_callback(Some(Arc::new(move || {
                *wakeups_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) += 1;
            })));

            let initial = tui.get_show_hardware_cursor();
            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .begin_output_capture();
            tui.set_show_hardware_cursor(!initial);
            let output = take_capture(&terminal);
            if initial {
                assert_eq!(output, HIDE_CURSOR);
            } else {
                assert!(output.is_empty());
            }
            assert_eq!(
                *wakeups.lock().unwrap_or_else(|error| error.into_inner()),
                1
            );
            assert_eq!(tui.take_render_request(), Some(false));

            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .begin_output_capture();
            tui.set_show_hardware_cursor(initial);
            let output = take_capture(&terminal);
            if initial {
                assert!(output.is_empty());
            } else {
                assert_eq!(output, HIDE_CURSOR);
            }
            assert_eq!(
                *wakeups.lock().unwrap_or_else(|error| error.into_inner()),
                2
            );
            assert_eq!(tui.take_render_request(), Some(false));
        }};
    }

    #[test]
    fn cursor_toggles_queue_owner_repaints_in_both_modes() {
        assert_cursor_repaint_contract!(TuiMainScreen::new);
        assert_cursor_repaint_contract!(TuiAltScreen::new);
    }
}
