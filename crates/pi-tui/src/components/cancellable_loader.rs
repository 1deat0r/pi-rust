//! CancellableLoader component — port of `packages/tui/src/components/cancellable-loader.ts`.
//!
//! A loader that can be cancelled with Escape, signalling an abort flag.

use crate::keys::TuiKey;
use crate::tui::Component;

/// A minimal loader with a cancel flag.
pub struct CancellableLoader {
    text: String,
    /// Set when the user presses Escape.
    pub aborted: bool,
    /// Called when the user presses Escape (if set).
    pub on_abort: Option<Box<dyn FnMut() + Send>>,
}

impl CancellableLoader {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            aborted: false,
            on_abort: None,
        }
    }

    pub fn cancel(&mut self) {
        if self.aborted {
            return;
        }
        self.aborted = true;
        if let Some(f) = &mut self.on_abort {
            f();
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for CancellableLoader {
    fn render(&self, width: usize) -> Vec<String> {
        let spinner = if self.aborted { "✗" } else { "…" };
        let line = format!("{spinner} {}", self.text);
        if width == 0 {
            return vec![line];
        }
        use crate::utils::slice_with_width;
        vec![slice_with_width(&line, width)]
    }

    fn handle_input(&mut self, key: &TuiKey) {
        if key.base == "escape" {
            self.cancel();
        }
    }
}
