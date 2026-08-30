#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_tui::components::Text;
use pi_tui::{
    Component, SharedComponent, StdinBuffer, TerminalBackend, TuiAltScreen, TuiKey, TuiStopOptions,
};

fn text(value: &str) -> SharedComponent {
    Arc::new(Mutex::new(Text::new(value, 0, 0, None)))
}

#[test]
fn burst_typing_preserves_every_character_in_order() {
    let mut buffer = StdinBuffer::new();
    let emitted = buffer.process("The quick brown fox 你好");

    let reconstructed = emitted.concat();
    assert_eq!(reconstructed, "The quick brown fox 你好");
    assert_eq!(emitted.len(), "The quick brown fox 你好".chars().count());
}

#[test]
fn escape_and_alt_sequences_are_not_lost_across_reads() {
    let mut buffer = StdinBuffer::new();
    assert!(buffer.process("\x1b").is_empty());
    assert_eq!(buffer.process("\r"), vec!["\x1b\r".to_string()]);

    let mut lone_escape = StdinBuffer::new();
    assert!(lone_escape.process("\x1b").is_empty());
    assert_eq!(lone_escape.flush(), vec!["\x1b".to_string()]);

    let mut alt_buffer = StdinBuffer::new();
    assert!(alt_buffer.process("\x1b").is_empty());
    assert_eq!(alt_buffer.process("x"), vec!["\x1bx".to_string()]);
}

#[test]
fn partial_csi_reads_emit_only_after_the_sequence_is_complete() {
    let mut buffer = StdinBuffer::new();
    assert!(buffer.process("\x1b[<35;20;").is_empty());
    assert!(buffer.process("5").is_empty());
    assert_eq!(buffer.process("m"), vec!["\x1b[<35;20;5m".to_string()]);
    assert!(buffer.flush().is_empty());
}

#[test]
fn redraw_requests_coalesce_and_force_is_retained() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
    let mut tui = TuiAltScreen::new(terminal);
    let wakeups = Arc::new(AtomicUsize::new(0));
    let wakeups_for_callback = Arc::clone(&wakeups);
    tui.set_request_render_callback(Some(Arc::new(move || {
        wakeups_for_callback.fetch_add(1, Ordering::SeqCst);
    })));

    tui.request_render(false);
    tui.request_render(false);
    tui.request_render(true);
    assert_eq!(wakeups.load(Ordering::SeqCst), 2);
    assert_eq!(tui.take_render_request(), Some(true));
    assert_eq!(tui.take_render_request(), None);
}

#[test]
fn unchanged_frames_do_not_write_a_second_terminal_frame() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
    let mut tui = TuiAltScreen::new(Arc::clone(&terminal));
    tui.add_child(text("stable transcript"));

    terminal.lock().unwrap().begin_output_capture();
    tui.render_now(true);
    let first = terminal.lock().unwrap().take_output_capture();
    assert!(!first.is_empty());

    terminal.lock().unwrap().begin_output_capture();
    tui.render_now(false);
    assert!(terminal.lock().unwrap().take_output_capture().is_empty());
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

#[test]
fn started_controller_queues_input_repaint_until_owner_frame() {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(20, 4)));
    let mut tui = TuiAltScreen::new(Arc::clone(&terminal));
    let probe = Arc::new(Mutex::new(EchoProbe {
        value: String::new(),
    }));
    tui.add_child(probe.clone());
    tui.set_focus(Some(probe.clone()));
    tui.start().unwrap();

    terminal.lock().unwrap().begin_output_capture();
    tui.dispatch_raw("a");
    tui.dispatch_raw("b");
    tui.dispatch_raw("c");

    assert_eq!(probe.lock().unwrap().value, "abc");
    assert!(tui.is_render_requested());
    assert!(terminal.lock().unwrap().take_output_capture().is_empty());

    terminal.lock().unwrap().begin_output_capture();
    tui.render_now(false);
    let output = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
    assert!(output.contains("abc"));
    assert!(!tui.is_render_requested());

    tui.stop(TuiStopOptions::default()).unwrap();
}
