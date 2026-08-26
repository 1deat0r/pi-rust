//! CancellableLoader component — port of the upstream cancellable loader.
//!
//! This type is a Loader with an abort signal and Escape/keybinding
//! cancellation. The signal is intentionally read-only to consumers; only
//! the component's cancellation path can transition it to aborted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::keybindings::get_keybindings;
use crate::keys::TuiKey;
use crate::tui::Component;

use super::loader::{Loader, LoaderIndicatorOptions, LoaderOptions, RequestRenderFn};

/// Read-only cancellation state exposed to work performed by a loader.
///
/// This is the Rust-native equivalent of the upstream AbortSignal. Cloning a
/// signal shares the same atomic state and is safe to poll from an async task
/// or worker thread.
#[derive(Clone, Debug)]
pub struct AbortSignal {
    aborted: Arc<AtomicBool>,
}

impl AbortSignal {
    fn new() -> Self {
        Self {
            aborted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return whether cancellation has been requested.
    pub fn aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Alias for callers that prefer predicate naming.
    pub fn is_aborted(&self) -> bool {
        self.aborted()
    }
}

/// A Loader that can be cancelled with the configured
/// tui.select.cancel keybinding.
pub struct CancellableLoader {
    loader: Loader,
    signal: AbortSignal,
    text: String,
    /// Compatibility field retained from the original Rust component.
    pub aborted: bool,
    /// Called once when the user first cancels the loader.
    ///
    /// The additional Sync bound keeps CancellableLoader usable as a shared
    /// TUI component. Closures assigned here still have the same direct
    /// Box::new(|...| ...) call-site shape.
    pub on_abort: Option<Box<dyn FnMut() + Send + Sync>>,
}

impl CancellableLoader {
    /// Construct a cancellable loader with the upstream default indicator.
    ///
    /// This signature is retained for existing pi-rust call sites.
    pub fn new(text: impl Into<String>) -> Self {
        Self::with_options(text, LoaderOptions::default())
    }

    /// Construct a cancellable loader with the same options as Loader.
    pub fn with_options(text: impl Into<String>, options: LoaderOptions) -> Self {
        let text = text.into();
        Self {
            loader: Loader::with_options(&text, options),
            signal: AbortSignal::new(),
            text,
            aborted: false,
            on_abort: None,
        }
    }

    /// Borrow the underlying loader for read-only inspection.
    pub fn loader(&self) -> &Loader {
        &self.loader
    }

    /// Return a clone of the shared abort signal.
    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Return whether this component has been cancelled.
    pub fn is_aborted(&self) -> bool {
        self.signal.aborted()
    }

    /// Cancel once and invoke the callback once.
    ///
    /// The atomic swap happens before taking the callback. This makes repeated
    /// Escape/Ctrl+C events idempotent and also ensures a callback panic cannot
    /// cause a later input event to invoke it a second time.
    pub fn cancel(&mut self) {
        if self.signal.aborted.swap(true, Ordering::AcqRel) {
            return;
        }
        self.aborted = true;
        if let Some(mut on_abort) = self.on_abort.take() {
            on_abort();
        }
    }

    /// Replace the loader message.
    pub fn set_message(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.loader.set_message(&message);
        self.text = message;
    }

    /// Replace the indicator and restart its animation.
    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        self.loader.set_indicator(indicator);
    }

    /// Start or restart the underlying animation without resetting the
    /// current frame.
    pub fn start(&mut self) {
        self.loader.start();
    }

    /// Stop the underlying animation.
    pub fn stop(&mut self) {
        self.loader.stop();
    }

    /// Stop animation and release its worker, matching upstream dispose.
    pub fn dispose(&mut self) {
        self.loader.stop();
    }

    /// Install the thread-safe repaint hook on the underlying loader.
    pub fn set_request_render_callback(&mut self, callback: Option<RequestRenderFn>) {
        self.loader.set_request_render_callback(callback);
    }

    /// Convenience repaint-hook setter.
    pub fn set_request_render<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.loader.set_request_render(callback);
    }

    /// Forward the deterministic frame-advance seam.
    pub fn advance_frame(&self) {
        self.loader.advance_frame();
    }

    /// Forward the normalized animation interval.
    pub fn interval(&self) -> std::time::Duration {
        self.loader.interval()
    }

    /// Forward the current frame index.
    pub fn current_frame(&self) -> usize {
        self.loader.current_frame()
    }

    /// Whether the underlying animation worker is running.
    pub fn is_running(&self) -> bool {
        self.loader.is_running()
    }

    /// Return the unstyled message.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for CancellableLoader {
    fn render(&self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let keybindings = get_keybindings();
        if keybindings.matches(key, "tui.select.cancel") {
            self.cancel();
        }
    }
}
