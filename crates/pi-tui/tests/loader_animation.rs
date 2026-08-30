#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use pi_tui::components::loader::{
    Loader, LoaderIndicatorOptions, LoaderOptions, DEFAULT_FRAMES, DEFAULT_INTERVAL_MS,
};
use pi_tui::components::CancellableLoader;
use pi_tui::{Component, TuiKey};

#[test]
fn loader_animation_contracts() {
    let loader = Loader::new("Working...");
    assert_eq!(
        loader.frames(),
        DEFAULT_FRAMES
            .iter()
            .map(|frame| (*frame).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        loader.interval(),
        Duration::from_millis(DEFAULT_INTERVAL_MS)
    );
    assert_eq!(loader.current_frame(), 0);

    let initial = loader.render(40);
    assert_eq!(initial.first().map(String::as_str), Some(""));
    assert!(initial.join("\n").contains("⠋"));
    assert!(initial.join("\n").contains("Working..."));

    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_hook = requests.clone();
    let options = LoaderOptions::default()
        .with_spinner_color(|frame| format!("<spinner>{frame}</spinner>"))
        .with_message_color(|message| format!("<message>{message}</message>"))
        .with_request_render(move || {
            requests_for_hook.fetch_add(1, Ordering::SeqCst);
        });
    let mut styled = Loader::with_options("styled", options);
    let styled_text = styled.render(80).join("\n");
    assert!(styled_text.contains("<spinner>⠋</spinner>"));
    assert!(styled_text.contains("<message>styled</message>"));
    let requests_after_start = requests.load(Ordering::SeqCst);
    assert!(requests_after_start >= 1);

    styled.stop();
    styled.advance_frame();
    assert_eq!(styled.current_frame(), 1);
    let frame_one = styled.render(80).join("\n");
    assert!(frame_one.contains("<spinner>⠙</spinner>"));
    assert!(requests.load(Ordering::SeqCst) > requests_after_start);

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default()
            .with_frames(["first", "second"])
            .with_interval_ms(-10),
    ));
    assert_eq!(styled.frames(), vec!["first", "second"]);
    assert_eq!(
        styled.interval(),
        Duration::from_millis(DEFAULT_INTERVAL_MS)
    );
    assert_eq!(styled.current_frame(), 0);
    styled.stop();
    let custom_text = styled.render(80).join("\n");
    assert!(custom_text.contains("first"));
    assert!(!custom_text.contains("<spinner>"));

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default().with_interval_ms(f64::NAN),
    ));
    assert_eq!(
        styled.interval(),
        Duration::from_millis(DEFAULT_INTERVAL_MS)
    );
    styled.stop();

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default().with_interval_ms(12.5),
    ));
    assert_eq!(styled.interval(), Duration::from_micros(12_500));
    styled.stop();

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default().with_interval_ms(f64::MIN_POSITIVE),
    ));
    assert_eq!(styled.interval(), Duration::from_millis(1));
    styled.stop();

    styled.set_indicator(Some(LoaderIndicatorOptions::default()));
    assert_eq!(styled.frames().len(), DEFAULT_FRAMES.len());
    assert_eq!(styled.current_frame(), 0);
    styled.stop();
    assert!(styled.render(80).join("\n").contains("⠋"));
    assert!(!styled.render(80).join("\n").contains("<spinner>"));

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default().with_frames(Vec::<String>::new()),
    ));
    styled.stop();
    assert!(styled.frames().is_empty());
    assert!(!styled.is_running());
    let hidden_text = styled.render(80).join("\n");
    assert!(hidden_text.contains("<message>styled</message>"));
    assert!(!hidden_text.contains("<spinner>"));

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default().with_frames(["only"]),
    ));
    assert_eq!(styled.current_frame(), 0);
    assert!(!styled.is_running());
    styled.advance_frame();
    assert_eq!(styled.current_frame(), 0);
    assert!(styled.render(80).join("\n").contains("only"));

    styled.set_indicator(Some(
        LoaderIndicatorOptions::default().with_frames(["a", "b", "c"]),
    ));
    styled.stop();
    styled.advance_frame();
    assert_eq!(styled.current_frame(), 1);
    styled.start();
    assert_eq!(styled.current_frame(), 1);
    assert!(styled.render(80).join("\n").contains("b"));
    styled.stop();

    styled.set_message("updated");
    assert_eq!(styled.message(), "updated");
    assert!(styled.render(80).join("\n").contains("updated"));

    // Verify the real interval worker wakes through the repaint hook without
    // using a sleep as a proxy for frame behavior.
    let (sender, receiver) = mpsc::channel();
    let mut timed = Loader::with_options(
        "timed",
        LoaderOptions::default()
            .with_indicator(Some(
                LoaderIndicatorOptions::default()
                    .with_frames(["zero", "one"])
                    .with_interval_ms(5),
            ))
            .with_request_render(move || {
                let _ = sender.send(());
            }),
    );
    receiver
        .recv_timeout(Duration::from_millis(100))
        .expect("loader must request its initial render");
    receiver
        .recv_timeout(Duration::from_millis(500))
        .expect("loader interval must request a repaint");
    assert_eq!(timed.current_frame(), 1);
    timed.stop();

    println!("LOADER_ANIMATION_TESTS_OK");
}

#[test]
fn cancellable_loader_contracts() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_for_hook = callback_count.clone();
    let mut loader = CancellableLoader::with_options(
        "cancel me",
        LoaderOptions::default().with_indicator(Some(
            LoaderIndicatorOptions::default().with_frames(["one", "two"]),
        )),
    );
    loader.on_abort = Some(Box::new(move || {
        callback_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));
    let signal = loader.signal();

    assert!(!signal.aborted());
    assert!(!loader.is_aborted());
    assert_eq!(loader.text(), "cancel me");
    assert!(loader.render(80).join("\n").contains("one"));
    assert!(loader.is_running());

    loader.handle_input(&TuiKey::simple("escape"));
    loader.handle_input(&TuiKey::simple("escape"));
    assert!(signal.aborted());
    assert!(signal.is_aborted());
    assert!(loader.is_aborted());
    assert!(loader.aborted);
    // The AbortSignal is idempotent, but upstream invokes onAbort for every
    // matching cancel input.
    assert_eq!(callback_count.load(Ordering::SeqCst), 2);

    loader.set_message("cancelled");
    assert_eq!(loader.text(), "cancelled");
    loader.advance_frame();
    assert!(loader.render(80).join("\n").contains("two"));

    loader.dispose();
    assert!(!loader.is_running());

    println!("CANCELLABLE_LOADER_TESTS_OK");
}
