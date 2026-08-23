//! TruncatedText component — single-line text truncated with an ellipsis.

use crate::tui::Component;
use crate::utils::{slice_with_width, visible_width};

pub struct TruncatedText {
    text: String,
    ellipsis: String,
}

impl TruncatedText {
    pub fn new(text: impl Into<String>, ellipsis: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ellipsis: ellipsis.into(),
        }
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }
}

impl Component for TruncatedText {
    fn render(&self, width: usize) -> Vec<String> {
        if visible_width(&self.text) <= width {
            return vec![self.text.clone()];
        }
        let ellipsis_width = visible_width(&self.ellipsis);
        let cut = width.saturating_sub(ellipsis_width);
        let truncated = slice_with_width(&self.text, cut);
        vec![format!("{truncated}{}", self.ellipsis)]
    }
}
