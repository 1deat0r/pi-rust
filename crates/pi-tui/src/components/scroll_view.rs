//! ScrollView component and retained layout-node state.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::keys::TuiKey;
use crate::layout::{
    LayoutNode, ScrollLayoutNode, ScrollLayoutState, ScrollOverscroll, ScrollbarMode,
};
use crate::tui::{Component, SharedComponent};

type RequestRenderCallback = Arc<dyn Fn() + Send + Sync>;

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
    scrollbar_hide_delay: Duration,
    scrollbar_last_activity: Option<Instant>,
    scrollbar_timer_generation: u64,
    scrollbar_timer_running: bool,
}

struct ScrollState {
    model: Arc<Mutex<ScrollModel>>,
    request_render: Arc<Mutex<Option<RequestRenderCallback>>>,
    follow_end: bool,
    primary: bool,
    overscroll: ScrollOverscroll,
}

impl ScrollState {
    fn new(follow_end: bool, primary: bool, overscroll: ScrollOverscroll) -> Self {
        Self {
            model: Arc::new(Mutex::new(ScrollModel {
                following_tail: follow_end,
                scrollbar: ScrollbarMode::Hidden,
                // Upstream ScrollView starts with transient scrollbars hidden
                // and only reveals them after viewport activity or hover.
                scrollbar_active: false,
                scrollbar_hide_delay: Duration::from_millis(1000),
                scrollbar_last_activity: None,
                scrollbar_timer_generation: 0,
                scrollbar_timer_running: false,
                ..ScrollModel::default()
            })),
            request_render: Arc::new(Mutex::new(None)),
            follow_end,
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
            ScrollbarMode::Always => Self::viewport_height_locked(model) > 0,
            ScrollbarMode::Auto => {
                (model.scrollbar_active || model.scrollbar_last_activity.is_some())
                    && model.content_height > Self::viewport_height_locked(model)
            }
        }
    }

    fn refresh_scrollbar_locked(model: &mut ScrollModel, now: Instant) -> bool {
        if model.scrollbar != ScrollbarMode::Auto || model.scrollbar_active {
            return false;
        }
        if model.scrollbar_last_activity.is_some_and(|started| {
            now.saturating_duration_since(started) >= model.scrollbar_hide_delay
        }) {
            model.scrollbar_last_activity = None;
            model.scrollbar_timer_generation = model.scrollbar_timer_generation.wrapping_add(1);
            model.scrollbar_timer_running = false;
            return true;
        }
        false
    }

    fn mark_scrollbar_activity(&self, now: Instant) {
        let (generation, delay) = {
            let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
            if model.scrollbar != ScrollbarMode::Auto
                || model.content_height <= Self::viewport_height_locked(&model)
            {
                return;
            }
            model.scrollbar_last_activity = Some(now);
            if model.scrollbar_active {
                return;
            }
            // Restart the hide deadline on every activity. A timer already
            // sleeping from an earlier scroll will observe the new generation
            // and exit; otherwise it could wake before the refreshed
            // deadline and leave the scrollbar visible without a repaint.
            model.scrollbar_timer_generation = model.scrollbar_timer_generation.wrapping_add(1);
            model.scrollbar_timer_running = true;
            (model.scrollbar_timer_generation, model.scrollbar_hide_delay)
        };

        let model = Arc::clone(&self.model);
        let request_render = Arc::clone(&self.request_render);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let hidden = {
                let mut model = model.lock().unwrap_or_else(|error| error.into_inner());
                if model.scrollbar_timer_generation != generation {
                    return;
                }
                model.scrollbar_timer_running = false;
                if model.scrollbar != ScrollbarMode::Auto
                    || model.scrollbar_active
                    || !model.scrollbar_last_activity.is_some_and(|started| {
                        Instant::now().saturating_duration_since(started)
                            >= model.scrollbar_hide_delay
                    })
                {
                    return;
                }
                model.scrollbar_last_activity = None;
                true
            };
            if hidden {
                let callback = request_render
                    .lock()
                    .ok()
                    .and_then(|callback| callback.clone());
                if let Some(callback) = callback {
                    callback();
                }
            }
        });
    }

    fn set_request_render_callback(&self, callback: Option<RequestRenderCallback>) {
        if let Ok(mut current) = self.request_render.lock() {
            *current = callback;
        }
    }

    fn request_render(&self) {
        let callback = self
            .request_render
            .lock()
            .ok()
            .and_then(|callback| callback.clone());
        if let Some(callback) = callback {
            callback();
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
        self.model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .effective_offset
    }

    fn primary(&self) -> bool {
        self.primary
    }

    fn overscroll(&self) -> ScrollOverscroll {
        self.overscroll
    }

    fn viewport_height(&self) -> usize {
        Self::viewport_height_locked(&self.model.lock().unwrap_or_else(|error| error.into_inner()))
    }

    fn is_following_end(&self) -> bool {
        self.model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .following_tail
    }

    fn update_layout(&self, content_height: usize, viewport_height: usize) {
        let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
        model.content_height = content_height;
        model.height = Some(viewport_height);
        if content_height <= viewport_height {
            model.scrollbar_last_activity = None;
        }
        Self::reconcile_locked(&mut model);
    }

    fn set_viewport_width(&self, width: usize) {
        self.model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .viewport_width = width.max(1);
    }

    fn get_content_width(&self, width: usize) -> usize {
        Self::content_width_locked(
            &self.model.lock().unwrap_or_else(|error| error.into_inner()),
            width,
        )
    }

    fn scrollbar_visible(&self) -> bool {
        let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
        Self::refresh_scrollbar_locked(&mut model, Instant::now());
        Self::scrollbar_visible_locked(&model)
    }

    fn scrollbar_style(&self, text: &str) -> String {
        format!("\x1b[100m{text}\x1b[49m")
    }

    fn scroll_to(&self, position: usize) {
        self.scroll_to_with_options(position, false);
    }

    fn scroll_to_with_options(&self, position: usize, disable_follow: bool) {
        let now = Instant::now();
        let (moved, following_changed) = {
            let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
            let previous_offset = model.effective_offset;
            let previous_following = model.following_tail;
            let max = Self::max_offset_locked(&model);
            model.offset = position.min(max);
            model.following_tail = !disable_follow && self.follow_end && model.offset == max;
            Self::reconcile_locked(&mut model);
            (
                model.effective_offset != previous_offset,
                model.following_tail != previous_following,
            )
        };
        if moved {
            self.mark_scrollbar_activity(now);
        }
        if moved || following_changed {
            self.request_render();
        }
    }

    fn set_request_render_callback(&self, callback: Option<Arc<dyn Fn() + Send + Sync>>) {
        ScrollState::set_request_render_callback(self, callback);
    }

    fn scroll_by(&self, lines: isize) -> isize {
        let now = Instant::now();
        let (unconsumed, moved, following_changed) = {
            let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
            let max = Self::max_offset_locked(&model);
            let current = if model.following_tail {
                max
            } else {
                model.offset.min(max)
            };
            let previous_following = model.following_tail;
            let target = if lines.is_negative() {
                current.saturating_sub(lines.unsigned_abs())
            } else {
                current.saturating_add(lines as usize).min(max)
            };
            model.offset = target;
            model.following_tail = self.follow_end && target == max;
            Self::reconcile_locked(&mut model);
            let unconsumed = if lines.is_negative() {
                -(lines
                    .unsigned_abs()
                    .saturating_sub(current.saturating_sub(target)) as isize)
            } else {
                lines.saturating_sub(target.saturating_sub(current) as isize)
            };
            (
                unconsumed,
                target != current,
                model.following_tail != previous_following,
            )
        };
        if moved {
            self.mark_scrollbar_activity(now);
        }
        if moved || following_changed {
            self.request_render();
        }
        unconsumed
    }

    fn scroll_to_start(&self) {
        let now = Instant::now();
        let (changed, moved) = {
            let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
            let previous_offset = model.effective_offset;
            let previous_following = model.following_tail;
            model.offset = 0;
            model.following_tail = self.follow_end && Self::max_offset_locked(&model) == 0;
            Self::reconcile_locked(&mut model);
            (
                model.following_tail != previous_following,
                model.effective_offset != previous_offset,
            )
        };
        if moved {
            self.mark_scrollbar_activity(now);
        }
        if changed || moved {
            self.request_render();
        }
    }

    fn scroll_to_end(&self) {
        let now = Instant::now();
        let (changed, moved) = {
            let mut model = self.model.lock().unwrap_or_else(|error| error.into_inner());
            let previous_offset = model.effective_offset;
            let previous_following = model.following_tail;
            model.offset = Self::max_offset_locked(&model);
            model.following_tail = self.follow_end;
            Self::reconcile_locked(&mut model);
            (
                model.following_tail != previous_following,
                model.effective_offset != previous_offset,
            )
        };
        if moved {
            self.mark_scrollbar_activity(now);
        }
        if changed || moved {
            self.request_render();
        }
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
        // Preserve the established Rust constructor used by the interactive
        // transcript, whose upstream counterpart is `follow: "end"`.
        Self::with_follow_options(child, true, primary, overscroll)
    }

    /// Construct a scroll view with an explicit upstream-style follow mode.
    /// Existing `with_options` callers retain follow-end behavior; passing
    /// `false` here represents upstream `follow: "none"` without changing
    /// the existing transcript API.
    pub fn with_follow_options(
        child: SharedComponent,
        follow_end: bool,
        primary: bool,
        overscroll: ScrollOverscroll,
    ) -> Self {
        Self {
            child,
            state: Arc::new(ScrollState::new(follow_end, primary, overscroll)),
        }
    }

    pub fn set_height(&mut self, height: usize) {
        let mut model = self
            .state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        model.height = Some(height);
        ScrollState::reconcile_locked(&mut model);
    }

    pub fn set_scrollbar(&mut self, mode: ScrollbarMode) {
        let should_mark_activity = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if model.scrollbar == mode {
                return;
            }
            model.scrollbar = mode;
            model.scrollbar_timer_generation = model.scrollbar_timer_generation.wrapping_add(1);
            model.scrollbar_timer_running = false;
            if mode != ScrollbarMode::Auto {
                model.scrollbar_last_activity = None;
                false
            } else {
                model.scrollbar_active
            }
        };
        if should_mark_activity {
            self.state.mark_scrollbar_activity(Instant::now());
        }
        self.state.request_render();
    }

    pub fn scrollbar(&self) -> ScrollbarMode {
        self.state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .scrollbar
    }

    pub fn set_scrollbar_active(&mut self, active: bool) {
        let changed = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if model.scrollbar_active == active {
                false
            } else {
                model.scrollbar_active = active;
                model.scrollbar_timer_generation = model.scrollbar_timer_generation.wrapping_add(1);
                model.scrollbar_timer_running = false;
                true
            }
        };
        if changed {
            self.state.mark_scrollbar_activity(Instant::now());
        }
    }

    /// Set the inactivity period for an automatic scrollbar.
    pub fn set_scrollbar_hide_delay(&mut self, delay: Duration) {
        self.state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .scrollbar_hide_delay = delay;
    }

    /// Apply a caller-supplied clock tick to the automatic scrollbar.
    /// Returns true when visibility may have changed.
    pub fn refresh_scrollbar(&mut self, now: Instant) -> bool {
        let changed = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            ScrollState::refresh_scrollbar_locked(&mut model, now)
        };
        if changed {
            self.state.request_render();
        }
        changed
    }

    pub fn is_scrollbar_visible(&self) -> bool {
        self.state.scrollbar_visible()
    }

    pub fn viewport_width(&self) -> usize {
        self.state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .viewport_width
    }

    pub fn viewport_height(&self) -> usize {
        ScrollState::viewport_height_locked(
            &self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    pub fn content_height(&self) -> usize {
        self.state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .content_height
    }

    pub fn scroll_top(&self) -> usize {
        self.state.scroll_top()
    }

    pub fn is_following_tail(&self) -> bool {
        self.state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .following_tail
    }

    pub fn is_following_end(&self) -> bool {
        self.is_following_tail()
    }

    pub fn scroll_to(&mut self, position: usize) {
        self.scroll_to_with_options(position, false);
    }

    /// Move to an absolute row, optionally preserving a manually revealed
    /// position at the content tail instead of re-enabling follow-end.
    pub fn scroll_to_with_options(&mut self, position: usize, disable_follow: bool) {
        let now = Instant::now();
        let (moved, following_changed) = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let previous_offset = model.effective_offset;
            let previous_following = model.following_tail;
            let max = ScrollState::max_offset_locked(&model);
            model.offset = position.min(max);
            model.following_tail = !disable_follow && self.state.follow_end && model.offset == max;
            ScrollState::reconcile_locked(&mut model);
            (
                model.effective_offset != previous_offset,
                model.following_tail != previous_following,
            )
        };
        if moved {
            self.state.mark_scrollbar_activity(now);
        }
        if moved || following_changed {
            self.state.request_render();
        }
    }

    pub fn scroll_to_start(&mut self) {
        let now = Instant::now();
        let (changed, moved) = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let previous_offset = model.effective_offset;
            let previous_following = model.following_tail;
            model.offset = 0;
            model.following_tail =
                self.state.follow_end && ScrollState::max_offset_locked(&model) == 0;
            ScrollState::reconcile_locked(&mut model);
            (
                model.following_tail != previous_following,
                model.effective_offset != previous_offset,
            )
        };
        if moved {
            self.state.mark_scrollbar_activity(now);
        }
        if changed || moved {
            self.state.request_render();
        }
    }

    pub fn scroll_to_end(&mut self) {
        let now = Instant::now();
        let (changed, moved) = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let previous_offset = model.effective_offset;
            let previous_following = model.following_tail;
            model.offset = ScrollState::max_offset_locked(&model);
            model.following_tail = self.state.follow_end;
            ScrollState::reconcile_locked(&mut model);
            (
                model.following_tail != previous_following,
                model.effective_offset != previous_offset,
            )
        };
        if moved {
            self.state.mark_scrollbar_activity(now);
        }
        if changed || moved {
            self.state.request_render();
        }
    }

    /// Compatibility alias for callers using the original Rust component API.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_to_end();
    }

    /// Scroll by a signed number of rows and return the unconsumed delta.
    pub fn scroll_by(&mut self, lines: isize) -> isize {
        let now = Instant::now();
        let (unconsumed, moved, following_changed) = {
            let mut model = self
                .state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let max = ScrollState::max_offset_locked(&model);
            let current = if model.following_tail {
                max
            } else {
                model.offset.min(max)
            };
            let previous_following = model.following_tail;
            let target = if lines.is_negative() {
                current.saturating_sub(lines.unsigned_abs())
            } else {
                current.saturating_add(lines as usize).min(max)
            };
            model.offset = target;
            model.following_tail = self.state.follow_end && target == max;
            ScrollState::reconcile_locked(&mut model);
            let unconsumed = if lines.is_negative() {
                -(lines
                    .unsigned_abs()
                    .saturating_sub(current.saturating_sub(target)) as isize)
            } else {
                lines.saturating_sub(target.saturating_sub(current) as isize)
            };
            (
                unconsumed,
                target != current,
                model.following_tail != previous_following,
            )
        };
        if moved {
            self.state.mark_scrollbar_activity(now);
        }
        if moved || following_changed {
            self.state.request_render();
        }
        unconsumed
    }

    fn refresh_content_metrics(&self) {
        let width = self.viewport_width().max(1);
        let content_width = self.state.get_content_width(width).max(1);
        let content_height = self
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(content_width)
            .len();
        self.state
            .update_layout(content_height, self.viewport_height());
    }
}

impl Component for ScrollView {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let content_width = self.state.get_content_width(width).max(1);
        let content = self
            .child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(content_width);
        let mut model = self
            .state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ScrollState::refresh_scrollbar_locked(&mut model, Instant::now());
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
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate();
    }

    fn handle_input(&mut self, key: &TuiKey) {
        match key.base.as_str() {
            "pageup" => {
                self.refresh_content_metrics();
                let mut model = self
                    .state
                    .model
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
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
                model.following_tail = self.state.follow_end && max == 0;
                ScrollState::reconcile_locked(&mut model);
            }
            "pagedown" => {
                self.refresh_content_metrics();
                let mut model = self
                    .state
                    .model
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
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
                model.following_tail = self.state.follow_end && model.offset == max;
                ScrollState::reconcile_locked(&mut model);
            }
            _ => {}
        }
        self.child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle_input(key);
    }

    fn set_height(&mut self, height: usize) {
        self.set_height(height);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::components::Text;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

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

    #[test]
    fn explicit_follow_none_stays_at_position_when_content_grows() {
        let child = text_child(8);
        let mut view =
            ScrollView::with_follow_options(child.clone(), false, true, ScrollOverscroll::Chain);
        view.set_height(3);

        assert!(!view.is_following_end());
        assert_eq!(trimmed(view.render(20)), ["line 1", "line 2", "line 3"]);
        assert_eq!(view.scroll_by(100), 95);
        assert_eq!(view.scroll_top(), 5);
        assert!(!view.is_following_end());

        child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_text(
                (1..=9)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        assert_eq!(trimmed(view.render(20)), ["line 6", "line 7", "line 8"]);
        assert!(!view.is_following_end());
    }

    #[test]
    fn disabling_follow_at_tail_keeps_search_reveal_pinned() {
        let child = text_child(8);
        let mut view = ScrollView::new(child.clone());
        view.set_height(3);
        view.render(20);

        view.scroll_to_with_options(5, true);
        assert_eq!(view.scroll_top(), 5);
        assert!(!view.is_following_end());

        child
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_text(
                (1..=9)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        assert_eq!(trimmed(view.render(20)), ["line 6", "line 7", "line 8"]);
        assert!(!view.is_following_end());
    }

    #[test]
    fn position_changes_request_repaint_but_noop_scroll_does_not() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        view.render(20);
        let notifications = Arc::new(AtomicUsize::new(0));
        let notifications_for_callback = notifications.clone();
        view.state
            .set_request_render_callback(Some(Arc::new(move || {
                notifications_for_callback.fetch_add(1, Ordering::SeqCst);
            })));

        view.scroll_to(0);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        view.scroll_by(0);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        view.scroll_to_end();
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn scrollbar_mode_changes_request_repaint_once_per_change() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        view.render(20);
        let notifications = Arc::new(AtomicUsize::new(0));
        let notifications_for_callback = notifications.clone();
        view.state
            .set_request_render_callback(Some(Arc::new(move || {
                notifications_for_callback.fetch_add(1, Ordering::SeqCst);
            })));

        view.set_scrollbar(ScrollbarMode::Auto);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        view.set_scrollbar(ScrollbarMode::Auto);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        view.set_scrollbar(ScrollbarMode::Always);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn repeated_auto_scrollbar_activity_rearms_the_hide_generation() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        view.render(20);
        view.set_scrollbar_hide_delay(Duration::from_secs(1));
        view.set_scrollbar(ScrollbarMode::Auto);

        let first_generation = {
            view.state.mark_scrollbar_activity(Instant::now());
            view.state
                .model
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .scrollbar_timer_generation
        };
        view.state
            .mark_scrollbar_activity(Instant::now() + Duration::from_millis(1));
        let second_generation = view
            .state
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .scrollbar_timer_generation;

        assert_eq!(second_generation, first_generation.wrapping_add(1));
        // Invalidate the short-lived test timers before dropping the view.
        view.set_scrollbar(ScrollbarMode::Hidden);
    }

    #[test]
    fn scrollbar_visibility_matches_upstream_at_single_column_width() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        view.set_scrollbar(ScrollbarMode::Always);
        let lines = view.render(1);
        assert!(view.is_scrollbar_visible());
        assert!(lines
            .iter()
            .all(|line| crate::utils::visible_width(line) <= 1));

        view.set_scrollbar(ScrollbarMode::Auto);
        view.scroll_by(-1);
        assert!(view.is_scrollbar_visible());
        view.set_scrollbar(ScrollbarMode::Hidden);
    }

    #[test]
    fn scrollbar_refresh_saturates_when_clock_precedes_activity() {
        let activity = Instant::now();
        let mut model = ScrollModel {
            content_height: 4,
            height: Some(2),
            viewport_width: 1,
            scrollbar: ScrollbarMode::Auto,
            scrollbar_last_activity: Some(activity),
            scrollbar_hide_delay: Duration::from_millis(10),
            ..ScrollModel::default()
        };
        let earlier = activity
            .checked_sub(Duration::from_millis(1))
            .unwrap_or(activity);
        assert!(!ScrollState::refresh_scrollbar_locked(&mut model, earlier));
        assert!(ScrollState::refresh_scrollbar_locked(
            &mut model,
            activity + Duration::from_millis(10)
        ));
        assert!(!ScrollState::scrollbar_visible_locked(&model));
    }

    #[test]
    fn scrollbar_expiry_uses_the_current_repaint_callback() {
        let mut view = ScrollView::new(text_child(8));
        view.set_height(3);
        view.render(20);
        view.set_scrollbar_hide_delay(Duration::from_millis(20));
        view.set_scrollbar(ScrollbarMode::Auto);

        let initial_notifications = Arc::new(AtomicUsize::new(0));
        let initial_for_callback = initial_notifications.clone();
        view.state
            .set_request_render_callback(Some(Arc::new(move || {
                initial_for_callback.fetch_add(1, Ordering::SeqCst);
            })));

        let (expired_tx, expired_rx) = std::sync::mpsc::channel();
        view.scroll_by(-1);
        view.state
            .set_request_render_callback(Some(Arc::new(move || {
                let _ = expired_tx.send(());
            })));

        assert!(expired_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        assert_eq!(initial_notifications.load(Ordering::SeqCst), 1);
        view.set_scrollbar(ScrollbarMode::Hidden);
    }
}
