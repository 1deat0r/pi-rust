//! Public TUI controller surfaces corresponding to upstream `TuiBase`,
//! `TuiMainScreen`, and `TuiAltScreen`.
//!
//! The controller owns terminal lifecycle, focus, overlays, retained layout,
//! and deterministic frame rendering.  It intentionally accepts raw strings
//! at the input boundary so existing consumers can keep parsing keyboard
//! sequences, while typed mouse reports are decoded before key dispatch.

use std::io;
use std::sync::{Arc, Mutex};

use crate::keys::{parse_key, TuiKey};
use crate::layout::{render_layout_frame, LayoutFrame};
use crate::mouse::{decode_mouse_event, MouseEvent};
use crate::terminal::{
    TerminalBackend, TerminalEvent, BEGIN_SYNC_UPDATE, CLEAR_SCREEN_HOME, END_SYNC_UPDATE,
};
use crate::tui::{
    Component, Container, OverlayHandle, OverlayManager, OverlayOptions, SharedComponent,
    CURSOR_MARKER,
};
use crate::utils::normalize_terminal_output;

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

struct ControllerCore {
    terminal: Arc<Mutex<TerminalBackend>>,
    root: Arc<Mutex<Container>>,
    overlays: OverlayManager,
    focused: Option<SharedComponent>,
    listeners: Arc<Mutex<Vec<(usize, InputListener)>>>,
    next_listener: usize,
    started: bool,
    show_hardware_cursor: bool,
    clear_on_shrink: bool,
    full_redraws: usize,
    previous_lines: Vec<String>,
    previous_width: usize,
    previous_height: usize,
    last_frame: Option<LayoutFrame>,
}

impl ControllerCore {
    fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            terminal,
            root: Arc::new(Mutex::new(Container::new())),
            overlays: OverlayManager::new(),
            focused: None,
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener: 0,
            started: false,
            show_hardware_cursor: std::env::var("PI_HARDWARE_CURSOR").ok().as_deref() == Some("1"),
            clear_on_shrink: std::env::var("PI_CLEAR_ON_SHRINK").ok().as_deref() == Some("1"),
            full_redraws: 0,
            previous_lines: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            last_frame: None,
        }
    }

    fn add_child(&mut self, component: SharedComponent) {
        self.root.lock().unwrap().add_child(component);
    }

    fn remove_child(&mut self, component: &SharedComponent) -> bool {
        self.root.lock().unwrap().remove_child(component)
    }

    fn clear(&mut self) {
        self.root.lock().unwrap().clear();
        self.focused = None;
    }

    fn set_focus(&mut self, component: Option<SharedComponent>) {
        if let Some(previous) = &self.focused {
            previous.lock().unwrap().set_focused(false);
        }
        if let Some(next) = &component {
            next.lock().unwrap().set_focused(true);
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
        listeners.lock().unwrap().push((id, Arc::new(listener)));
        InputListenerHandle { id, listeners }
    }

    fn dispatch_mouse(&mut self, event: &MouseEvent) {
        if self.overlays.has_visible_overlay() {
            self.overlays.dispatch_mouse(event);
        } else if let Some(focused) = &self.focused {
            focused.lock().unwrap().handle_mouse(event);
        }
    }

    fn dispatch_key(&mut self, key: &TuiKey) {
        if self.overlays.has_visible_overlay() {
            self.overlays.dispatch(key);
        } else if let Some(focused) = &self.focused {
            focused.lock().unwrap().handle_input(key);
        }
    }

    fn dispatch_raw(&mut self, raw: &str) {
        let listeners = self.listeners.lock().unwrap().clone();
        if listeners.iter().any(|(_, listener)| listener(raw)) {
            return;
        }
        if let Ok(Some(event)) = decode_mouse_event(raw) {
            self.dispatch_mouse(&event);
        } else {
            self.dispatch_key(&parse_key(raw));
        }
    }

    fn render_component(&mut self, root: SharedComponent, mode: TuiMode, force: bool) {
        let (width, height) = {
            let terminal = self.terminal.lock().unwrap();
            (terminal.width().max(1), terminal.height().max(1))
        };
        let mut frame = render_layout_frame(root, width, height);
        frame.lines = self.overlays.composite(&frame.lines, width, height);
        let mut lines = std::mem::take(&mut frame.lines)
            .into_iter()
            .map(|line| normalize_terminal_output(&line).replace(CURSOR_MARKER, ""))
            .collect::<Vec<_>>();
        lines.resize(height, String::new());
        lines.truncate(height);
        let changed_dimensions = width != self.previous_width || height != self.previous_height;
        let full = force || self.previous_lines.is_empty() || changed_dimensions;
        if full {
            self.full_redraws = self.full_redraws.saturating_add(1);
        }
        let mut terminal = self.terminal.lock().unwrap();
        terminal.write_raw(BEGIN_SYNC_UPDATE);
        if full || (self.clear_on_shrink && lines.len() < self.previous_lines.len()) {
            terminal.write_raw(CLEAR_SCREEN_HOME);
        } else {
            terminal.write_raw("\x1b[H");
        }
        for (row, line) in lines.iter().enumerate() {
            if !full && self.previous_lines.get(row) == Some(line) {
                continue;
            }
            terminal.write_raw(&format!(
                "\x1b[{};1H\x1b[2K{}",
                row + 1,
                crate::layout::clamp_layout_line(line, width)
            ));
        }
        terminal.write_raw(&format!("\x1b[{};1H", height));
        terminal.write_raw(END_SYNC_UPDATE);
        if mode == TuiMode::Regular {
            terminal.write_raw("\r");
        }
        drop(terminal);
        self.previous_lines = lines;
        self.previous_width = width;
        self.previous_height = height;
        self.last_frame = Some(frame);
    }

    fn start(&mut self, mode: TuiMode) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let use_alt = mode == TuiMode::Fullscreen;
        if let Err(error) = self
            .terminal
            .lock()
            .unwrap()
            .enter_raw_with_alt_screen(use_alt)
        {
            self.started = false;
            return Err(error);
        }
        Ok(())
    }

    fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        if !self.started {
            return Ok(());
        }
        self.started = false;
        if !options.preserve_screen {
            self.terminal.lock().unwrap().clear_screen();
        }
        self.terminal.lock().unwrap().leave_raw()
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
        self.core.show_hardware_cursor = enabled;
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
        self.core.overlays.show_overlay(component, options)
    }
    pub fn hide_overlay(&mut self, handle: OverlayHandle) -> bool {
        self.core.overlays.hide(handle)
    }
    pub fn add_input_listener(
        &mut self,
        listener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> InputListenerHandle {
        self.core.add_input_listener(listener)
    }
    pub fn dispatch_raw(&mut self, raw: &str) {
        self.core.dispatch_raw(raw);
    }
    pub fn dispatch_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::Key(raw) => self.dispatch_raw(&raw),
            TerminalEvent::Resize(width, height) => {
                self.core.terminal.lock().unwrap().set_size(width, height);
                self.render_now(true);
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
            max_lines_rendered: self.core.previous_lines.len(),
            previous_viewport_top: 0,
        }
    }
    pub fn restore_render_state(&mut self, state: TuiMainScreenRenderState) {
        self.core.previous_lines = state.previous_lines;
        self.core.previous_width = state.previous_width;
        self.core.previous_height = state.previous_height;
    }
}

impl Component for TuiMainScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.core.root.lock().unwrap().render(width)
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
}

impl TuiAltScreen {
    pub fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            core: ControllerCore::new(terminal),
            layout_root: None,
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
        self.layout_root = component;
    }
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.core.show_hardware_cursor
    }
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.core.show_hardware_cursor = enabled;
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
        self.core.overlays.show_overlay(component, options)
    }
    pub fn hide_overlay(&mut self, handle: OverlayHandle) -> bool {
        self.core.overlays.hide(handle)
    }
    pub fn add_input_listener(
        &mut self,
        listener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> InputListenerHandle {
        self.core.add_input_listener(listener)
    }
    pub fn dispatch_raw(&mut self, raw: &str) {
        self.core.dispatch_raw(raw);
    }
    pub fn dispatch_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::Key(raw) => self.dispatch_raw(&raw),
            TerminalEvent::Resize(width, height) => {
                self.core.terminal.lock().unwrap().set_size(width, height);
                self.render_now(true);
            }
        }
    }
    pub fn start(&mut self) -> io::Result<()> {
        let result = self.core.start(TuiMode::Fullscreen);
        if result.is_ok() {
            self.render_now(true);
        }
        result
    }
    pub fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        self.core.stop(options)
    }
    pub fn render_now(&mut self, force: bool) {
        let root = self
            .layout_root
            .clone()
            .unwrap_or_else(|| self.core.root.clone());
        self.core.render_component(root, TuiMode::Fullscreen, force);
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
        self.render_now(false);
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
        self.render_now(false);
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
        self.render_now(false);
    }
}

impl Component for TuiAltScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.layout_root
            .clone()
            .unwrap_or_else(|| self.core.root.clone())
            .lock()
            .unwrap()
            .render(width)
    }
    fn handle_input(&mut self, key: &TuiKey) {
        self.core.dispatch_key(key);
    }
    fn handle_mouse(&mut self, event: &MouseEvent) {
        self.core.dispatch_mouse(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;

    fn text(value: &str) -> SharedComponent {
        Arc::new(Mutex::new(Text::new(value, 0, 0, None)))
    }

    #[test]
    fn main_and_alt_controllers_render_resize_and_cleanup_idempotently() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
        let mut main = TuiMainScreen::new(terminal.clone());
        main.add_child(text("main"));
        main.render_now(true);
        assert_eq!(main.full_redraws(), 1);
        terminal.lock().unwrap().set_size(8, 2);
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
        assert_eq!(modal.lock().unwrap().keys, 1);
        assert_eq!(modal.lock().unwrap().mice, 1);
        assert!(tui.hide_overlay(handle));
        assert!(!tui.has_overlay());
    }
}
