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
    /// Forward mouse events inside the border to the child with
    /// border-relative coordinates (upstream 0.85.1 `Box.handleMouse`:
    /// content offset, child-height hit test). Border rows/columns do not
    /// forward.
    fn handle_mouse(&mut self, event: &crate::mouse::MouseEvent) {
        if event.y == 0 || event.x == 0 {
            return;
        }
        let forwarded = crate::mouse::MouseEvent {
            x: event.x - 1,
            y: event.y - 1,
            ..*event
        };
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle_mouse(&forwarded);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod box_mouse_tests {
    use super::*;
    use crate::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseModifiers};
    use crate::tui::Component;
    use std::sync::{Arc, Mutex};

    struct Probe {
        seen: Arc<Mutex<Vec<(usize, usize)>>>,
    }

    impl Component for Probe {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["hi".to_string()]
        }
        fn handle_mouse(&mut self, event: &MouseEvent) {
            self.seen.lock().unwrap().push((event.x, event.y));
        }
    }

    fn click(x: usize, y: usize) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Press,
            button: MouseButton::Left,
            x,
            y,
            modifiers: MouseModifiers {
                shift: false,
                alt: false,
                ctrl: false,
            },
            screen_x: 0,
            screen_y: 0,
            width: 0,
            height: 0,
        }
    }

    #[test]
    fn box_forwards_mouse_inside_border_with_translated_coords() {
        // 0.85.1 upstream `Box.handleMouse`: events inside the border reach
        // the child with border-relative coordinates; border clicks do not.
        // RED: Rust Box has no mouse forwarding.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::new(Mutex::new(Probe { seen: seen.clone() }));
        let mut boxed = Box::new(probe, None);
        // Row 1 col 2 at width 10: inside the border (row 0/2 are borders).
        boxed.handle_mouse(&click(2, 1));
        assert_eq!(*seen.lock().unwrap(), vec![(1, 0)]);
        // Border row: no forwarding.
        boxed.handle_mouse(&click(2, 0));
        assert_eq!(*seen.lock().unwrap(), vec![(1, 0)]);
    }
}
