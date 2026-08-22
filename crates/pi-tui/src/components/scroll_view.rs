//! ScrollView component — a windowed viewport over a child.

use crate::keys::TuiKey;
use crate::tui::{Component, SharedComponent};

pub struct ScrollView {
    pub child: SharedComponent,
    pub offset: usize,
    height: Option<usize>,
}

impl ScrollView {
    pub fn new(child: SharedComponent) -> Self {
        Self { child, offset: 0, height: None }
    }
    pub fn set_height(&mut self, height: usize) {
        self.height = Some(height);
    }
    pub fn scroll_to_bottom(&mut self) {
        self.offset = usize::MAX; // clamped on next render using child height
    }
}

impl Component for ScrollView {
    fn render(&self, width: usize) -> Vec<String> {
        let content = self.child.lock().unwrap().render(width);
        match self.height {
            Some(height) => {
                if content.len() <= height {
                    return content;
                }
                let max_offset = content.len() - height;
                let offset = if self.offset == usize::MAX { max_offset } else { self.offset.min(max_offset) };
                content[offset..offset + height].to_vec()
            }
            None => content,
        }
    }
    fn invalidate(&mut self) {
        self.child.lock().unwrap().invalidate();
    }
    fn handle_input(&mut self, key: &TuiKey) {
        match key.base.as_str() {
            "pageup" => {
                let step = self.height.unwrap_or(10).saturating_sub(1);
                self.offset = self.offset.saturating_sub(step);
            }
            "pagedown" => {
                self.offset = self.offset.saturating_add(self.height.unwrap_or(10).saturating_sub(1));
            }
            _ => {}
        }
        self.child.lock().unwrap().handle_input(key);
    }
}
