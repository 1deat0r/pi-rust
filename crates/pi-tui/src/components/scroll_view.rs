//! ScrollView component and retained layout-node state.

use std::sync::{Arc, Mutex};

use crate::keys::TuiKey;
use crate::layout::{
    LayoutNode, ScrollLayoutNode, ScrollLayoutState, ScrollOverscroll, ScrollbarMode,
};
use crate::tui::{Component, SharedComponent};

#[derive(Debug, Default)]
struct ScrollModel {
    offset: usize,
    height: Option<usize>,
    viewport_width: usize,
    content_height: usize,
    effective_offset: usize,
    following_tail: bool,
    scrollbar: ScrollbarMode,
    scrollbar_active: bool,
}

#[derive(Debug)]
struct ScrollState {
    model: Mutex<ScrollModel>,
    primary: bool,
    overscroll: ScrollOverscroll,
}

impl ScrollState {
    fn new(primary: bool, overscroll: ScrollOverscroll) -> Self {
        Self {
            model: Mutex::new(ScrollModel {
                following_tail: true,
                scrollbar: ScrollbarMode::Hidden,
                scrollbar_active: true,
                ..ScrollModel::default()
            }),
            primary,
            overscroll,
        }
    }

    fn viewport_height_locked(model: &ScrollModel) -> usize {
        model.height.unwrap_or(model.content_height)
    }

    fn max_offset_locked(model: &ScrollModel) -> usize {
        model
            .content_height
            .saturating_sub(Self::viewport_height_locked(model))
    }

    fn reconcile_locked(model: &mut ScrollModel) {
        let max_offset = Self::max_offset_locked(model);
        model.effective_offset = if model.following_tail {
            max_offset
        } else {
            model.offset.min(max_offset)
        };
    }

    fn scrollbar_visible_locked(model: &ScrollModel) -> bool {
        match model.scrollbar {
            ScrollbarMode::Hidden => false,
            ScrollbarMode::Always => model.viewport_width > 1,
            ScrollbarMode::Auto => {
                model.scrollbar_active
                    && model.content_height > Self::viewport_height_locked(model)
                    && model.viewport_width > 1
            }
        }
    }

    fn content_width_locked(model: &ScrollModel, width: usize) -> usize {
        if model.scrollbar == ScrollbarMode::Always && width > 1 {
            width.saturating_sub(1).max(1)
        } else {
            width.max(1)
        }
    }
}

impl ScrollLayoutState for ScrollState {
    fn scroll_top(&self) -> usize {
        self.model.lock().unwrap().effective_offset
    }

    fn primary(&self) -> bool {
        self.primary
    }

    fn overscroll(&self) -> ScrollOverscroll {
        self.overscroll
    }

    fn viewport_height(&self) -> usize {
        Self::viewport_height_locked(&self.model.lock().unwrap())
    }

    fn is_following_end(&self) -> bool {
        self.model.lock().unwrap().following_tail
    }

    fn update_layout(&self, content_height: usize, viewport_height: usize) {
        let mut model = self.model.lock().unwrap();
        model.content_height = content_height;
        model.height = Some(viewport_height);
        Self::reconcile_locked(&mut model);
    }

    fn get_content_width(&self, width: usize) -> usize {
        Self::content_width_locked(&self.model.lock().unwrap(), width)
    }

    fn scrollbar_visible(&self) -> bool {
        Self::scrollbar_visible_locked(&self.model.lock().unwrap())
    }

    fn scrollbar_style(&self, text: &str) -> String {
        format!("\x1b[100m{text}\x1b[49m")
    }

    fn scroll_by(&self, lines: isize) -> isize {
        let mut model = self.model.lock().unwrap();
        let max = Self::max_offset_locked(&model);
        let current = if model.following_tail {
            max
        } else {
            model.offset.min(max)
        };
        let target = if lines.is_negative() {
            current.saturating_sub(lines.unsigned_abs())
        } else {
            current.saturating_add(lines as usize).min(max)
        };
        model.offset = target;
        model.following_tail = target == max;
        Self::reconcile_locked(&mut model);
        if lines.is_negative() {
            -(lines
                .unsigned_abs()
                .saturating_sub(current.saturating_sub(target)) as isize)
        } else {
            lines.saturating_sub(target.saturating_sub(current) as isize)
        }
    }

    fn scroll_to_start(&self) {
        let mut model = self.model.lock().unwrap();
        model.offset = 0;
        model.following_tail = Self::max_offset_locked(&model) == 0;
        Self::reconcile_locked(&mut model);
    }

    fn scroll_to_end(&self) {
        let mut model = self.model.lock().unwrap();
        model.offset = Self::max_offset_locked(&model);
        model.following_tail = true;
        Self::reconcile_locked(&mut model);
    }
}

/// A scrollable child with stable geometry state shared by the layout engine.
pub struct ScrollView {
    pub child: SharedComponent,
    state: Arc<ScrollState>,
}

impl ScrollView {
    pub fn new(child: SharedComponent) -> Self {
        Self::with_options(child, false, ScrollOverscroll::Chain)
    }

    pub fn with_options(
        child: SharedComponent,
        primary: bool,
        overscroll: ScrollOverscroll,
    ) -> Self {
        Self {
            child,
            state: Arc::new(ScrollState::new(primary, overscroll)),
        }
    }

    pub fn set_height(&mut self, height: usize) {
        let mut model = self.state.model.lock().unwrap();
        model.height = Some(height);
        ScrollState::reconcile_locked(&mut model);
    }

    pub fn set_scrollbar(&mut self, mode: ScrollbarMode) {
        self.state.model.lock().unwrap().scrollbar = mode;
    }

    pub fn scrollbar(&self) -> ScrollbarMode {
        self.state.model.lock().unwrap().scrollbar
    }

    pub fn set_scrollbar_active(&mut self, active: bool) {
        self.state.model.lock().unwrap().scrollbar_active = active;
    }

    pub fn viewport_width(&self) -> usize {
        self.state.model.lock().unwrap().viewport_width
    }

    pub fn viewport_height(&self) -> usize {
        ScrollState::viewport_height_locked(&self.state.model.lock().unwrap())
    }

    pub fn content_height(&self) -> usize {
        self.state.model.lock().unwrap().content_height
    }

    pub fn scroll_top(&self) -> usize {
        self.state.scroll_top()
    }

    pub fn is_following_tail(&self) -> bool {
        self.state.model.lock().unwrap().following_tail
    }

    pub fn is_following_end(&self) -> bool {
        self.is_following_tail()
    }

    pub fn scroll_to(&mut self, position: usize) {
        let mut model = self.state.model.lock().unwrap();
        let max = ScrollState::max_offset_locked(&model);
        model.offset = position.min(max);
        model.following_tail = model.offset == max;
        ScrollState::reconcile_locked(&mut model);
    }

    pub fn scroll_to_start(&mut self) {
        let mut model = self.state.model.lock().unwrap();
        model.offset = 0;
        model.following_tail = ScrollState::max_offset_locked(&model) == 0;
        ScrollState::reconcile_locked(&mut model);
    }

    pub fn scroll_to_end(&mut self) {
        let mut model = self.state.model.lock().unwrap();
        model.offset = ScrollState::max_offset_locked(&model);
        model.following_tail = true;
        ScrollState::reconcile_locked(&mut model);
    }

    /// Compatibility alias for callers using the original Rust component API.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_to_end();
    }

    /// Scroll by a signed number of rows and return the unconsumed delta.
    pub fn scroll_by(&mut self, lines: isize) -> isize {
        let mut model = self.state.model.lock().unwrap();
        let max = ScrollState::max_offset_locked(&model);
        let current = if model.following_tail {
            max
        } else {
            model.offset.min(max)
        };
        let target = if lines.is_negative() {
            current.saturating_sub(lines.unsigned_abs())
        } else {
            current.saturating_add(lines as usize).min(max)
        };
        model.offset = target;
        model.following_tail = target == max;
        ScrollState::reconcile_locked(&mut model);
        if lines.is_negative() {
            -(lines
                .unsigned_abs()
                .saturating_sub(current.saturating_sub(target)) as isize)
        } else {
            lines.saturating_sub(target.saturating_sub(current) as isize)
        }
    }

    fn refresh_content_metrics(&self) {
        let width = self.viewport_width().max(1);
        let content_width = self.state.get_content_width(width).max(1);
        let content_height = self.child.lock().unwrap().render(content_width).len();
        self.state
            .update_layout(content_height, self.viewport_height());
    }
}

impl Component for ScrollView {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let content_width = self.state.get_content_width(width).max(1);
        let content = self.child.lock().unwrap().render(content_width);
        let mut model = self.state.model.lock().unwrap();
        model.viewport_width = width;
        model.content_height = content.len();
        let viewport_height = ScrollState::viewport_height_locked(&model);
        let max_offset = content.len().saturating_sub(viewport_height);
        let offset = if model.following_tail {
            max_offset
        } else {
            model.offset.min(max_offset)
        };
        model.effective_offset = offset;
        if viewport_height == 0 {
            return Vec::new();
        }
        let mut visible = if content.len() <= viewport_height {
            content
        } else {
            content[offset..offset + viewport_height].to_vec()
        };
        if content_width < width {
            for line in &mut visible {
                line.push(' ');
            }
        }
        visible
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Scroll(ScrollLayoutNode {
            component: self.child.clone(),
            state: self.state.clone(),
        }))
    }

    fn invalidate(&mut self) {
        self.child.lock().unwrap().invalidate();
    }

    fn handle_input(&mut self, key: &TuiKey) {
        match key.base.as_str() {
            "pageup" => {
                self.refresh_content_metrics();
                let mut model = self.state.model.lock().unwrap();
                let max = ScrollState::max_offset_locked(&model);
                let current = if model.following_tail {
                    max
                } else {
                    model.offset.min(max)
                };
                let step = ScrollState::viewport_height_locked(&model)
                    .saturating_sub(1)
                    .max(1);
                model.offset = current.saturating_sub(step);
                model.following_tail = max == 0;
                ScrollState::reconcile_locked(&mut model);
            }
            "pagedown" => {
                self.refresh_content_metrics();
                let mut model = self.state.model.lock().unwrap();
                let max = ScrollState::max_offset_locked(&model);
                let current = if model.following_tail {
                    max
                } else {
                    model.offset.min(max)
                };
                let step = ScrollState::viewport_height_locked(&model)
                    .saturating_sub(1)
                    .max(1);
                model.offset = current.saturating_add(step).min(max);
                model.following_tail = model.offset == max;
                ScrollState::reconcile_locked(&mut model);
            }
            _ => {}
        }
        self.child.lock().unwrap().handle_input(key);
    }

    fn set_height(&mut self, height: usize) {
        self.set_height(height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;
    use std::sync::{Arc, Mutex};

    fn text_child(lines: usize) -> Arc<Mutex<Text>> {
        let text = (1..=lines)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        Arc::new(Mutex::new(Text::new(text, 0, 0, None)))
    }
    fn key(name: &str) -> TuiKey {
        TuiKey::simple(name)
    }
    fn trimmed(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    #[test]
    fn layout_state_is_shared_and_sync_safe() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ScrollView>();
        let view = ScrollView::new(text_child(4));
        assert!(view.layout_node().is_some());
    }

    #[test]
    fn empty_content_has_zero_geometry_and_no_visible_rows() {
        let child = Arc::new(Mutex::new(Text::new("", 0, 0, None)));
        let mut view = ScrollView::new(child);
        view.set_height(4);
        assert!(view.render(32).is_empty());
        assert_eq!(view.viewport_width(), 32);
        assert_eq!(view.viewport_height(), 4);
        assert_eq!(view.content_height(), 0);
        assert_eq!(view.scroll_top(), 0);
        assert!(view.is_following_tail());
    }

    #[test]
    fn page_navigation_reuses_width_and_restores_tail() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        assert_eq!(trimmed(view.render(20)), ["line 6", "line 7", "line 8"]);
        view.handle_input(&key("pageup"));
        assert_eq!(trimmed(view.render(20)), ["line 4", "line 5", "line 6"]);
        view.handle_input(&key("pagedown"));
        assert_eq!(trimmed(view.render(20)), ["line 6", "line 7", "line 8"]);
        assert!(view.is_following_end());
    }

    #[test]
    fn signed_scroll_returns_unconsumed_delta_at_edges() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        view.render(20);
        assert_eq!(view.scroll_by(-100), -95);
        assert_eq!(view.scroll_top(), 0);
        assert_eq!(view.scroll_by(100), 95);
        assert_eq!(view.scroll_top(), 5);
    }
}
