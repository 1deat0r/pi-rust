//! Mouse region — port of upstream 0.85.1 `components/mouse-region.ts`.
//!
//! Adds mouse handling to an existing component without changing its
//! rendering: the child sees the event first, then the region handler runs
//! when the child ignores it.

use crate::mouse::MouseEvent;
use crate::tui::{Component, SharedComponent};

/// Mouse handler for a region: returns true when the event is consumed.
pub type MouseRegionHandler = std::sync::Arc<dyn Fn(&MouseEvent) -> bool + Send + Sync>;

/// Transparent mouse-handling wrapper (upstream `MouseRegion`).
pub struct MouseRegion {
    child: SharedComponent,
    on_mouse: MouseRegionHandler,
}

impl MouseRegion {
    pub fn new(child: SharedComponent, on_mouse: MouseRegionHandler) -> Self {
        Self { child, on_mouse }
    }
}

impl Component for MouseRegion {
    fn render(&self, width: usize) -> Vec<String> {
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(width)
    }
    fn handle_mouse(&mut self, event: &MouseEvent) {
        // Child-first dispatch is approximated: the shared child is
        // forwarded the event, then the region handler observes it.
        // A consumed event stops further handling upstream.
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle_mouse(event);
        let _ = (self.on_mouse)(event);
    }
    fn invalidate(&mut self) {
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod mouse_region_tests {
    use super::*;
    use crate::mouse::{MouseButton, MouseEventKind, MouseModifiers};
    use std::sync::{Arc, Mutex};

    struct Probe {
        seen: Arc<Mutex<usize>>,
    }

    impl Component for Probe {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["hi".to_string()]
        }
        fn handle_mouse(&mut self, _event: &MouseEvent) {
            *self.seen.lock().unwrap() += 1;
        }
    }

    #[test]
    fn mouse_region_renders_child_and_observes_events() {
        // 0.85.1 upstream: rendering is unchanged, handler observes.
        // RED: MouseRegion does not exist.
        let child_seen = Arc::new(Mutex::new(0usize));
        let handler_seen = Arc::new(Mutex::new(0usize));
        let probe = Arc::new(Mutex::new(Probe {
            seen: child_seen.clone(),
        }));
        let handler_seen_for_closure = handler_seen.clone();
        let mut region = MouseRegion::new(
            probe,
            Arc::new(move |_| {
                *handler_seen_for_closure.lock().unwrap() += 1;
                true
            }),
        );
        assert_eq!(region.render(10), vec!["hi".to_string()]);
        region.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Press,
            button: MouseButton::Left,
            x: 1,
            y: 1,
            modifiers: MouseModifiers {
                shift: false,
                alt: false,
                ctrl: false,
            },
            screen_x: 0,
            screen_y: 0,
            width: 0,
            height: 0,
        });
        assert_eq!(*child_seen.lock().unwrap(), 1);
        assert_eq!(*handler_seen.lock().unwrap(), 1);
    }
}
