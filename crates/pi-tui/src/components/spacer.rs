//! Spacer component — port of `packages/tui/src/components/spacer.ts`.

use crate::tui::Component;

pub struct Spacer {
    height: usize,
}

impl Spacer {
    pub fn new(height: usize) -> Self {
        Self { height }
    }
    pub fn set_height(&mut self, height: usize) {
        self.height = height;
    }
}

impl Component for Spacer {
    fn render(&self, width: usize) -> Vec<String> {
        vec![" ".repeat(width); self.height]
    }
}
