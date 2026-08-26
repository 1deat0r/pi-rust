use std::sync::{Arc, Mutex};

use pi_tui::components::Text;
use pi_tui::{
    decode_mouse_event, Component, MouseButton, MouseEvent, MouseEventKind, MouseModifiers,
    SharedComponent, TerminalBackend, TuiAltScreen, TuiKey, TuiStopOptions,
};

#[derive(Default)]
struct Probe {
    keys: Vec<TuiKey>,
    mice: Vec<MouseEvent>,
    focused: bool,
}

impl Component for Probe {
    fn render(&self, _width: usize) -> Vec<String> {
        vec!["probe".to_string()]
    }
    fn handle_input(&mut self, key: &TuiKey) {
        self.keys.push(key.clone());
    }
    fn handle_mouse(&mut self, event: &MouseEvent) {
        self.mice.push(*event);
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[test]
fn typed_mouse_events_dispatch_without_faking_provider_turns() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(30, 5)));
    let mut tui = TuiAltScreen::new(terminal);
    let probe = Arc::new(Mutex::new(Probe::default()));
    let component: SharedComponent = probe.clone();
    tui.add_child(component.clone());
    tui.set_focus(Some(component));
    tui.dispatch_raw("x");
    tui.dispatch_raw("\x1b[<0;7;3M");
    let probe = probe.lock().unwrap();
    assert_eq!(probe.keys.len(), 1);
    assert_eq!(probe.mice.len(), 1);
    assert_eq!(probe.mice[0].button, MouseButton::Left);
    assert_eq!((probe.mice[0].x, probe.mice[0].y), (6, 2));
}

#[test]
fn listener_can_consume_input_and_handle_partial_reports_at_decoder_boundary() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(30, 5)));
    let mut tui = TuiAltScreen::new(terminal);
    let probe = Arc::new(Mutex::new(Probe::default()));
    let component: SharedComponent = probe.clone();
    tui.add_child(component.clone());
    tui.set_focus(Some(component));
    let _listener = tui.add_input_listener(|raw| raw == "consume");
    tui.dispatch_raw("consume");
    assert_eq!(probe.lock().unwrap().keys.len(), 0);
    assert_eq!(
        decode_mouse_event("\x1b[<64;2;2"),
        Err(pi_tui::MouseDecodeError::Incomplete)
    );
    tui.dispatch_raw("\x1b[<64;2;2M");
    assert_eq!(probe.lock().unwrap().mice[0].kind, MouseEventKind::WheelUp);
}

#[test]
fn controller_resize_rerenders_and_stop_is_safe_before_and_after_start() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(12, 3)));
    let mut tui = TuiAltScreen::new(terminal.clone());
    tui.add_child(Arc::new(Mutex::new(Text::new("wide text", 0, 0, None))));
    tui.stop(TuiStopOptions::default()).unwrap();
    tui.render_now(true);
    terminal.lock().unwrap().set_size(4, 2);
    tui.dispatch_event(pi_tui::TerminalEvent::Resize(4, 2));
    assert_eq!(tui.full_redraws(), 2);
    tui.stop(TuiStopOptions {
        preserve_screen: true,
    })
    .unwrap();
}

#[test]
fn malformed_mouse_reports_never_become_pointer_events() {
    assert_eq!(
        decode_mouse_event("\x1b[<0;0;1M"),
        Err(pi_tui::MouseDecodeError::Malformed)
    );
    assert_eq!(
        decode_mouse_event("\x1b[<0;1;1X"),
        Err(pi_tui::MouseDecodeError::Incomplete)
    );
    assert_eq!(
        decode_mouse_event("\x1b[M\x01!!"),
        Err(pi_tui::MouseDecodeError::Malformed)
    );
    let _ = MouseModifiers::default();
}
