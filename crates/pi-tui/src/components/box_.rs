//! Box component — bordered container.

use crate::tui::{Component, SharedComponent};
use crate::utils::visible_width;

pub struct Box {
    pub child: SharedComponent,
    pub title: Option<String>,
}

impl Box {
    pub fn new(child: SharedComponent, title: Option<String>) -> Self {
        Self { child, title }
    }
}

impl Component for Box {
    fn render(&self, width: usize) -> Vec<String> {
        let inner_width = width.saturating_sub(2);
        let child_lines = self
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(inner_width);
        let mut lines = Vec::new();
        let title = self.title.clone().unwrap_or_default();
        if title.is_empty() {
            lines.push(format!("╭{}╮", "─".repeat(width.saturating_sub(2))));
        } else {
            let t = format!(" {title} ");
            let rest = width.saturating_sub(visible_width(&t) + 2);
            lines.push(format!("╭{}{}╮", t, "─".repeat(rest)));
        }
        for child_line in child_lines {
            let visible = visible_width(&child_line);
            let pad = inner_width.saturating_sub(visible);
            lines.push(format!("│{}{}│", child_line, " ".repeat(pad)));
        }
        lines.push(format!("╰{}╯", "─".repeat(width.saturating_sub(2))));
        lines
    }
    fn invalidate(&mut self) {
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate();
    }
}
