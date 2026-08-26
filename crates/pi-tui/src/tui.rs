//! Component model + differential renderer — port of `packages/tui/src/tui.ts`
//! (the subset the interactive mode uses: component tree, per-line render,
//! input dispatch, diff-based terminal output).

use std::sync::{Arc, Mutex};

use crate::keys::TuiKey;
use crate::layout::LayoutNode;
use crate::mouse::MouseEvent;
use crate::terminal::{TerminalBackend, BEGIN_SYNC_UPDATE, CLEAR_SCREEN_HOME, END_SYNC_UPDATE};
use crate::utils::{
    extract_segments, normalize_terminal_output, slice_by_column_strict, slice_with_width_info,
    visible_width,
};

/// Zero-width APC marker emitted by focused input components. The renderer
/// removes it before writing the line and positions the terminal cursor at
/// its visual column, matching the upstream IME cursor contract.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// A component renders to lines for a viewport width.
pub trait Component {
    fn render(&self, width: usize) -> Vec<String>;
    fn handle_input(&mut self, _key: &TuiKey) {}
    /// Optional retained layout metadata. Components without a node keep the
    /// intrinsic line-rendering behavior used by the original Rust port.
    fn layout_node(&self) -> Option<LayoutNode> {
        None
    }
    /// Optional typed pointer-event hook. Raw terminal compatibility remains
    /// available through `handle_input`; controllers call this hook only after
    /// a sequence has decoded as a mouse event.
    fn handle_mouse(&mut self, _event: &MouseEvent) {}
    fn invalidate(&mut self) {}
    fn set_focused(&mut self, _focused: bool) {}
    /// Optional height hint used by scene layout. Components that implement a
    /// viewport can retain the allocated height; fixed-height components
    /// leave the default unchanged.
    fn set_height(&mut self, _height: usize) {}
}

pub type SharedComponent = Arc<Mutex<dyn Component + Send + Sync>>;

/// A component container matching the upstream TUI's child lifecycle.
pub struct Container {
    pub children: Vec<SharedComponent>,
}

impl Container {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    pub fn add_child(&mut self, component: SharedComponent) {
        self.children.push(component);
    }

    pub fn remove_child(&mut self, component: &SharedComponent) -> bool {
        let Some(index) = self
            .children
            .iter()
            .position(|candidate| Arc::ptr_eq(candidate, component))
        else {
            return false;
        };
        self.children.remove(index);
        true
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        self.children
            .iter()
            .flat_map(|child| child.lock().unwrap().render(width))
            .collect()
    }

    fn invalidate(&mut self) {
        for child in &self.children {
            child.lock().unwrap().invalidate();
        }
    }

    fn set_focused(&mut self, focused: bool) {
        for child in &self.children {
            child.lock().unwrap().set_focused(focused);
        }
    }

    fn set_height(&mut self, height: usize) {
        for child in &self.children {
            child.lock().unwrap().set_height(height);
        }
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Stack(crate::layout::StackLayoutNode {
            direction: crate::layout::LayoutDirection::Vertical,
            entries: self
                .children
                .iter()
                .cloned()
                .map(crate::layout::StackLayoutEntry::new)
                .collect(),
            gap: 0,
            align: crate::layout::LayoutAlign::Stretch,
        }))
    }
}

/// Anchor used by an overlay's layout resolver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverlayAnchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// A cell count or a percentage of the relevant terminal dimension.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Cells(usize),
    Percent(f64),
}

impl From<usize> for SizeValue {
    fn from(value: usize) -> Self {
        Self::Cells(value)
    }
}

impl SizeValue {
    fn resolve(self, reference: usize) -> usize {
        match self {
            Self::Cells(value) => value,
            Self::Percent(percent) => {
                ((reference as f64 * percent.clamp(0.0, 100.0)) / 100.0).floor() as usize
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayMargin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

impl From<usize> for OverlayMargin {
    fn from(value: usize) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// Positioning and sizing options for an overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<usize>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: isize,
    pub offset_y: isize,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: OverlayMargin,
    pub non_capturing: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            width: None,
            min_width: None,
            max_height: None,
            anchor: OverlayAnchor::Center,
            offset_x: 0,
            offset_y: 0,
            row: None,
            col: None,
            margin: OverlayMargin::default(),
            non_capturing: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayRect {
    pub width: usize,
    pub row: usize,
    pub col: usize,
    pub max_height: Option<usize>,
}

fn anchor_row(anchor: OverlayAnchor, height: usize, available: usize, margin: usize) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => margin,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            margin + available.saturating_sub(height)
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            margin + available.saturating_sub(height) / 2
        }
    }
}

fn anchor_col(anchor: OverlayAnchor, width: usize, available: usize, margin: usize) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => margin,
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            margin + available.saturating_sub(width)
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            margin + available.saturating_sub(width) / 2
        }
    }
}

/// Resolve an overlay's final terminal rectangle. Percentages are relative to
/// the terminal, while anchors and margins keep the complete overlay inside
/// the available viewport.
pub fn resolve_overlay_layout(
    options: &OverlayOptions,
    overlay_height: usize,
    term_width: usize,
    term_height: usize,
) -> OverlayRect {
    let margin = options.margin;
    let available_width = term_width
        .saturating_sub(margin.left.saturating_add(margin.right))
        .max(1);
    let available_height = term_height
        .saturating_sub(margin.top.saturating_add(margin.bottom))
        .max(1);
    let mut width = options
        .width
        .map(|value| value.resolve(term_width))
        .unwrap_or_else(|| available_width.min(80));
    if let Some(min_width) = options.min_width {
        width = width.max(min_width);
    }
    width = width.clamp(1, available_width);
    let max_height = options
        .max_height
        .map(|value| value.resolve(term_height).clamp(1, available_height));
    let effective_height = max_height.unwrap_or(overlay_height).min(available_height);

    let mut row = options
        .row
        .map(|value| match value {
            SizeValue::Cells(value) => value,
            SizeValue::Percent(percent) => {
                margin.top
                    + ((available_height.saturating_sub(effective_height) as f64
                        * percent.clamp(0.0, 100.0))
                        / 100.0) as usize
            }
        })
        .unwrap_or_else(|| {
            anchor_row(
                options.anchor,
                effective_height,
                available_height,
                margin.top,
            )
        });
    let mut col = options
        .col
        .map(|value| match value {
            SizeValue::Cells(value) => value,
            SizeValue::Percent(percent) => {
                margin.left
                    + ((available_width.saturating_sub(width) as f64 * percent.clamp(0.0, 100.0))
                        / 100.0) as usize
            }
        })
        .unwrap_or_else(|| anchor_col(options.anchor, width, available_width, margin.left));
    row = row.saturating_add_signed(options.offset_y);
    col = col.saturating_add_signed(options.offset_x);
    let max_row = term_height
        .saturating_sub(margin.bottom)
        .saturating_sub(effective_height);
    let max_col = term_width
        .saturating_sub(margin.right)
        .saturating_sub(width);
    OverlayRect {
        width,
        row: row.clamp(margin.top, max_row),
        col: col.clamp(margin.left, max_col),
        max_height,
    }
}

/// Composite one overlay line into a base line, preserving style state on
/// both sides and refusing to split a wide grapheme at the overlay boundary.
pub fn composite_tui_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    if crate::terminal_image::is_image_line(base_line) {
        return base_line.to_string();
    }
    let after_start = start_col.saturating_add(overlay_width);
    let base = extract_segments(
        base_line,
        start_col,
        after_start,
        total_width.saturating_sub(after_start),
        true,
    );
    let overlay = slice_with_width_info(overlay_line, 0, overlay_width, true);
    let before_pad = start_col.saturating_sub(base.before_width);
    let overlay_pad = overlay_width.saturating_sub(overlay.width);
    let actual_before_width = start_col.max(base.before_width);
    let actual_overlay_width = overlay_width.max(overlay.width);
    let after_target = total_width.saturating_sub(actual_before_width + actual_overlay_width);
    let after_pad = after_target.saturating_sub(base.after_width);
    let mut result = format!(
        "{}{}{}{}{}{}{}{}",
        base.before,
        " ".repeat(before_pad),
        SEGMENT_RESET,
        overlay.text,
        " ".repeat(overlay_pad),
        SEGMENT_RESET,
        base.after,
        " ".repeat(after_pad)
    );
    // The suffix is reopened with the base style, but a source reset may sit
    // just beyond the visible terminal width. Close this segment explicitly
    // so foreground/italic/hyperlink state cannot bleed into the next row.
    if base.after.contains("\x1b[") || base.after.contains("\x1b]8;") {
        result.push_str("\x1b[0m");
    }
    if visible_width(&result) <= total_width {
        result
    } else {
        slice_by_column_strict(&result, 0, total_width)
    }
}

/// Handle for a live entry in [`OverlayManager`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayHandle {
    id: usize,
}

impl OverlayHandle {
    pub fn id(self) -> usize {
        self.id
    }
}

struct OverlayEntry {
    component: SharedComponent,
    options: OverlayOptions,
    hidden: bool,
    focus_order: usize,
}

/// Overlay stack with upstream-style capture and focus semantics.
pub struct OverlayManager {
    entries: Vec<OverlayEntry>,
    focused: Option<usize>,
    next_id: usize,
}

impl OverlayManager {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            focused: None,
            next_id: 0,
        }
    }

    pub fn show_overlay(
        &mut self,
        component: SharedComponent,
        options: OverlayOptions,
    ) -> OverlayHandle {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let captures = !options.non_capturing;
        self.entries.push(OverlayEntry {
            component,
            options,
            hidden: false,
            focus_order: id,
        });
        let handle = OverlayHandle { id };
        if captures {
            self.focus(handle);
        }
        handle
    }

    fn index_of(&self, handle: OverlayHandle) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.focus_order == handle.id)
    }

    pub fn hide(&mut self, handle: OverlayHandle) -> bool {
        let Some(index) = self.index_of(handle) else {
            return false;
        };
        let was_focused = self.focused == Some(handle.id);
        self.entries.remove(index);
        if was_focused {
            self.focused = self
                .topmost_capturing_visible()
                .map(|entry| entry.focus_order);
            self.apply_focus();
        }
        true
    }

    pub fn set_hidden(&mut self, handle: OverlayHandle, hidden: bool) -> bool {
        let Some(index) = self.index_of(handle) else {
            return false;
        };
        self.entries[index].hidden = hidden;
        if hidden && self.focused == Some(handle.id) {
            self.focused = self
                .topmost_capturing_visible()
                .map(|entry| entry.focus_order);
            self.apply_focus();
        }
        true
    }

    pub fn is_hidden(&self, handle: OverlayHandle) -> bool {
        self.index_of(handle)
            .and_then(|index| self.entries.get(index))
            .is_none_or(|entry| entry.hidden)
    }

    pub fn focus(&mut self, handle: OverlayHandle) -> bool {
        let Some(index) = self.index_of(handle) else {
            return false;
        };
        if self.entries[index].hidden {
            return false;
        }
        self.focused = Some(handle.id);
        self.apply_focus();
        true
    }

    pub fn unfocus(&mut self, handle: OverlayHandle) -> bool {
        if self.focused != Some(handle.id) {
            return false;
        }
        self.focused = self
            .entries
            .iter()
            .filter(|entry| {
                !entry.hidden && !entry.options.non_capturing && entry.focus_order != handle.id
            })
            .max_by_key(|entry| entry.focus_order)
            .map(|entry| entry.focus_order);
        self.apply_focus();
        true
    }

    pub fn is_focused(&self, handle: OverlayHandle) -> bool {
        self.focused == Some(handle.id)
    }

    pub fn has_visible_overlay(&self) -> bool {
        self.entries.iter().any(|entry| !entry.hidden)
    }

    pub fn focused_component(&self) -> Option<SharedComponent> {
        let id = self.focused?;
        self.entries
            .iter()
            .find(|entry| entry.focus_order == id)
            .map(|entry| entry.component.clone())
    }

    pub fn dispatch(&mut self, key: &TuiKey) {
        if let Some(component) = self.focused_component() {
            component.lock().unwrap().handle_input(key);
        }
    }

    pub fn dispatch_mouse(&mut self, event: &MouseEvent) {
        if let Some(component) = self.focused_component() {
            component.lock().unwrap().handle_mouse(event);
        }
    }

    /// Composite visible overlays in stack order into a screen-sized frame.
    pub fn composite(
        &self,
        lines: &[String],
        term_width: usize,
        term_height: usize,
    ) -> Vec<String> {
        let mut result = lines.to_vec();
        let mut rendered = Vec::new();
        let mut minimum_lines = result.len();
        for entry in self.entries.iter().filter(|entry| !entry.hidden) {
            let preliminary = resolve_overlay_layout(&entry.options, 0, term_width, term_height);
            let mut overlay_lines = entry.component.lock().unwrap().render(preliminary.width);
            if let Some(max_height) = preliminary.max_height {
                overlay_lines.truncate(max_height);
            }
            let rect = resolve_overlay_layout(
                &entry.options,
                overlay_lines.len(),
                term_width,
                term_height,
            );
            minimum_lines = minimum_lines.max(rect.row + overlay_lines.len());
            rendered.push((overlay_lines, rect));
        }
        let working_height = result.len().max(term_height).max(minimum_lines);
        result.resize(working_height, String::new());
        let viewport_start = working_height.saturating_sub(term_height);
        for (overlay_lines, rect) in rendered {
            for (offset, overlay_line) in overlay_lines.iter().enumerate() {
                let index = viewport_start + rect.row + offset;
                if let Some(base_line) = result.get_mut(index) {
                    *base_line = composite_tui_line(
                        base_line,
                        overlay_line,
                        rect.col,
                        rect.width,
                        term_width,
                    );
                }
            }
        }
        result
    }

    fn topmost_capturing_visible(&self) -> Option<&OverlayEntry> {
        self.entries
            .iter()
            .filter(|entry| !entry.hidden && !entry.options.non_capturing)
            .max_by_key(|entry| entry.focus_order)
    }

    fn apply_focus(&self) {
        for entry in &self.entries {
            entry
                .component
                .lock()
                .unwrap()
                .set_focused(self.focused == Some(entry.focus_order));
        }
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A scene: fixed-size children + a grower, laid out in order.
pub struct Scene {
    pub children: Vec<SharedComponent>,
    pub grow_index: Option<usize>,
}

impl Scene {
    pub fn new(children: Vec<SharedComponent>, grow_index: Option<usize>) -> Self {
        Self {
            children,
            grow_index,
        }
    }
    fn render(self: &Scene, width: usize, height: usize) -> Vec<String> {
        let grow_index = self.grow_index.filter(|index| *index < self.children.len());
        let fixed_height = self
            .children
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != grow_index)
            .map(|(_, child)| {
                let lines = child.lock().unwrap().render(width).len();
                lines
            })
            .sum::<usize>();
        if let Some(index) = grow_index {
            let allocated = height.saturating_sub(fixed_height);
            self.children[index]
                .lock()
                .unwrap()
                .set_height(allocated.max(1));
        }
        let mut lines: Vec<String> = Vec::new();
        for child in &self.children {
            let child_lines = child.lock().unwrap().render(width);
            lines.extend(child_lines);
        }
        if lines.is_empty() {
            lines.push(" ".repeat(width));
        }
        // Pad to the requested number of lines.
        while lines.len() < height.max(1) {
            lines.push(" ".repeat(width));
        }
        lines.truncate(height);
        lines
    }
}

/// The tree renderer: diffs consecutive frames and writes the minimal
/// per-line updates to the terminal.
pub struct Tree {
    terminal: Arc<Mutex<TerminalBackend>>,
    last_lines: Vec<String>,
    last_screen_epoch: Option<u64>,
    last_width: Option<usize>,
    last_height: Option<usize>,
    force_full_redraw: bool,
    focused: Option<SharedComponent>,
}

impl Tree {
    pub fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            terminal,
            last_lines: Vec::new(),
            last_screen_epoch: None,
            last_width: None,
            last_height: None,
            force_full_redraw: true,
            focused: None,
        }
    }

    /// Access to the terminal backend (for raw event reads).
    pub fn terminal_handle(&self) -> Arc<Mutex<TerminalBackend>> {
        self.terminal.clone()
    }

    /// Query the cell dimensions used by image components. This is a no-op
    /// for terminals without Kitty/iTerm2 image support.
    pub fn query_cell_size(&mut self) -> bool {
        self.terminal.lock().unwrap().query_cell_size()
    }

    /// Feed a terminal response to the cell-size parser. A successful update
    /// invalidates the previous frame so image components recompute their
    /// row/column allocation on the next render.
    pub fn consume_cell_size_response(&mut self, data: &str) -> bool {
        let consumed = self
            .terminal
            .lock()
            .unwrap()
            .consume_cell_size_response(data);
        if consumed {
            self.force_full_redraw = true;
        }
        consumed
    }

    /// Force the next render to redraw every line after the terminal size
    /// changes. Terminals and multiplexers may clear or reposition the
    /// visible screen while delivering a resize event, so a differential
    /// frame based on the old dimensions is not safe to reuse.
    pub fn invalidate(&mut self) {
        self.force_full_redraw = true;
    }

    pub fn leave_alt_screen(&mut self) {
        let mut term = self.terminal.lock().unwrap();
        let _ = term.leave_raw();
    }

    pub fn focus(&mut self, component: SharedComponent) {
        if let Some(previous) = &self.focused {
            previous.lock().unwrap().set_focused(false);
        }
        component.lock().unwrap().set_focused(true);
        self.focused = Some(component);
    }

    /// Render the scene, diffing against the previous frame.
    pub fn render(&mut self, scene: Option<&Arc<Mutex<Scene>>>) {
        let (width, height, screen_epoch) = {
            let term = self.terminal.lock().unwrap();
            (term.width(), term.height(), term.screen_epoch())
        };
        if self.last_screen_epoch != Some(screen_epoch)
            || self.last_width != Some(width)
            || self.last_height != Some(height)
        {
            self.force_full_redraw = true;
            self.last_screen_epoch = Some(screen_epoch);
        }
        let mut rendered_lines: Vec<String> = match scene {
            Some(scene) => {
                let guard = scene.lock().unwrap();
                guard.render(width, height)
            }
            None => vec![" ".repeat(width); height],
        }
        .into_iter()
        .collect();
        let cursor_position = extract_cursor_position(&mut rendered_lines, height);
        let lines: Vec<String> = rendered_lines
            .into_iter()
            .map(|line| normalize_terminal_output(&line))
            .collect();
        let force_full_redraw = self.force_full_redraw;
        self.diff_render(&lines, force_full_redraw, cursor_position);
        self.last_lines = lines;
        self.last_width = Some(width);
        self.last_height = Some(height);
        self.force_full_redraw = false;
    }

    fn diff_render(
        &mut self,
        lines: &[String],
        force_full_redraw: bool,
        cursor_position: Option<(usize, usize)>,
    ) {
        let term = self.terminal.clone();
        let mut t = term.lock().unwrap();
        // Match the upstream renderer's synchronized-output transaction so a
        // multi-line frame is presented atomically by terminals and tmux.
        t.write_raw(BEGIN_SYNC_UPDATE);
        let common = self.last_lines.len().min(lines.len());
        // A resize, screen transition or explicit invalidation means the
        // terminal may no longer contain the previous frame. Clear before
        // replaying every line so stale rows cannot survive a shrink.
        if force_full_redraw {
            t.write_raw(CLEAR_SCREEN_HOME);
        } else {
            t.write_raw("\x1b[H");
        }
        for (i, line) in lines.iter().enumerate() {
            let same = !force_full_redraw && i < common && self.last_lines[i] == *line;
            if same {
                continue;
            }
            if i > 0 {
                t.write_raw(&format!("\x1b[{};1H", i + 1));
            }
            let term_width = t.width();
            t.write_raw(&format!(
                "\x1b[2K{}",
                truncate_for_terminal(line, term_width)
            ));
        }
        // Clear remaining old lines if the frame shrank.
        if lines.len() < self.last_lines.len() {
            for i in lines.len()..self.last_lines.len() {
                t.write_raw(&format!("\x1b[{};1H\x1b[2K", i + 1));
            }
        }
        t.write_raw(&format!("\x1b[{};1H", lines.len().max(1)));
        if let Some((row, col)) = cursor_position {
            t.write_raw(&format!("\x1b[{};{}H", row + 1, col + 1));
        }
        t.write_raw(END_SYNC_UPDATE);
        let _ = &mut self.focused;
    }

    /// Dispatch terminal input to the focused component.
    pub fn dispatch(&mut self, key: &TuiKey) {
        if let Some(focused) = &self.focused {
            let mut guard = focused.lock().unwrap();
            guard.handle_input(key);
        }
    }

    /// Dispatch a typed mouse event to the focused component.
    pub fn dispatch_mouse(&mut self, event: &MouseEvent) {
        if let Some(focused) = &self.focused {
            focused.lock().unwrap().handle_mouse(event);
        }
    }
}

/// Extract and remove the focused component's hardware-cursor marker from a
/// rendered frame. Only the visible viewport is searched, and the bottommost
/// marker wins just like the upstream renderer when nested layouts contain
/// more than one focusable component.
fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let viewport_start = lines.len().saturating_sub(height);
    let position = (viewport_start..lines.len()).rev().find_map(|row| {
        let marker_index = lines[row].find(CURSOR_MARKER)?;
        Some((row, visible_width(&lines[row][..marker_index])))
    });
    if let Some((row, col)) = position {
        for line in lines.iter_mut() {
            if line.contains(CURSOR_MARKER) {
                *line = line.replace(CURSOR_MARKER, "");
            }
        }
        Some((row, col))
    } else {
        None
    }
}

fn truncate_for_terminal(line: &str, width: usize) -> String {
    if visible_width(line) <= width {
        return line.to_string();
    }
    slice_by_column_strict(line, 0, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text::Text;

    #[test]
    fn scene_renders_children() {
        let text = Arc::new(Mutex::new(Text::new("hello", 0, 0, None)));
        let scene = Scene::new(vec![text], None);
        let lines = scene.render(10, 1);
        assert!(visible_width(&lines[0]) >= 5);
    }

    struct TestLines {
        lines: Vec<String>,
        focused: bool,
    }

    impl Component for TestLines {
        fn render(&self, _width: usize) -> Vec<String> {
            self.lines.clone()
        }

        fn set_focused(&mut self, focused: bool) {
            self.focused = focused;
        }
    }

    fn shared_lines(lines: &[&str]) -> SharedComponent {
        Arc::new(Mutex::new(TestLines {
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
            focused: false,
        }))
    }

    #[test]
    fn composite_preserves_terminal_width_at_cjk_boundary() {
        let result = composite_tui_line("1234567890", "界", 3, 2, 10);
        assert_eq!(visible_width(&result), 10);
        assert!(result.contains("界"));

        // A one-cell overlay slot cannot contain half of a two-cell glyph.
        let strict = composite_tui_line("1234567890", "界", 3, 1, 10);
        assert!(visible_width(&strict) <= 10);
        assert!(!strict.contains('界'));
    }

    #[test]
    fn extract_segments_drop_cjk_that_crosses_the_overlay_start() {
        let segments = crate::utils::extract_segments("abcd让EFGH", 5, 9, 11, true);
        assert_eq!(segments.before, "abcd");
        assert_eq!(segments.before_width, 4);
        assert_eq!(segments.after, "H");
        assert_eq!(segments.after_width, 1);

        let result = composite_tui_line("abcd让EFGH", "│XX│", 5, 4, 20);
        assert!(!result.contains('让'));
        assert_eq!(visible_width(&result), 20);
        assert_eq!(visible_width(&slice_by_column_strict(&result, 5, 4)), 4);
    }

    #[test]
    fn composite_resets_overlay_style_before_restoring_base_style() {
        let base = "\x1b[31mabcdefgh\x1b[0m";
        let result = composite_tui_line(base, "X", 2, 2, 8);
        assert_eq!(visible_width(&result), 8);
        assert!(result.contains(SEGMENT_RESET));
        assert!(result.contains("efgh"));
        assert!(result.contains("\x1b[31m"));
    }

    #[test]
    fn overlay_layout_supports_percentages_margins_and_clamping() {
        let options = OverlayOptions {
            width: Some(SizeValue::Percent(50.0)),
            max_height: Some(SizeValue::Percent(50.0)),
            anchor: OverlayAnchor::BottomRight,
            margin: OverlayMargin::from(1),
            ..OverlayOptions::default()
        };
        let rect = resolve_overlay_layout(&options, 20, 80, 24);
        assert_eq!(rect.width, 40);
        assert_eq!(rect.max_height, Some(12));
        assert_eq!(rect.col, 39);
        assert_eq!(rect.row, 11);
    }

    #[test]
    fn overlay_manager_captures_focus_and_hides_cleanly() {
        let mut manager = OverlayManager::new();
        let first = shared_lines(&["first"]);
        let second = shared_lines(&["second"]);
        let first_handle = manager.show_overlay(first.clone(), OverlayOptions::default());
        assert!(manager.is_focused(first_handle));
        let second_handle = manager.show_overlay(
            second.clone(),
            OverlayOptions {
                non_capturing: true,
                ..OverlayOptions::default()
            },
        );
        assert!(manager.is_focused(first_handle));
        assert!(!manager.is_focused(second_handle));
        assert!(manager.set_hidden(first_handle, true));
        assert!(!manager.is_focused(first_handle));
        assert!(manager.set_hidden(first_handle, false));
        assert!(manager.focus(first_handle));
        assert!(manager.hide(first_handle));
        assert!(manager.has_visible_overlay());
        assert!(!manager.is_focused(second_handle));
        assert!(manager.hide(second_handle));
        assert!(!manager.has_visible_overlay());
    }

    #[test]
    fn tree_forces_full_redraw_on_first_frame_and_resize() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(8, 2)));
        terminal.lock().unwrap().begin_output_capture();
        let mut tree = Tree::new(terminal.clone());
        let child = shared_lines(&["12345678", "abcdefgh"]);
        let scene = Arc::new(Mutex::new(Scene::new(vec![child], None)));

        tree.render(Some(&scene));
        let first = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
        assert!(first.contains(CLEAR_SCREEN_HOME));
        assert!(first.contains("12345678"));
        assert!(first.contains("abcdefgh"));

        terminal.lock().unwrap().begin_output_capture();
        tree.render(Some(&scene));
        let unchanged = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
        assert!(!unchanged.contains("12345678"));
        assert!(!unchanged.contains("abcdefgh"));

        terminal.lock().unwrap().set_size(4, 1);
        terminal.lock().unwrap().begin_output_capture();
        tree.render(Some(&scene));
        let resized = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
        assert!(resized.contains(CLEAR_SCREEN_HOME));
        assert!(resized.contains("1234"));
        assert!(!resized.contains("abcdefgh"));
    }

    #[test]
    fn tree_extracts_cursor_marker_and_restores_hardware_cursor_column() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(8, 2)));
        terminal.lock().unwrap().begin_output_capture();
        let mut tree = Tree::new(terminal.clone());
        let child: SharedComponent = Arc::new(Mutex::new(TestLines {
            lines: vec![format!("abc{CURSOR_MARKER}def")],
            focused: false,
        }));
        let scene = Arc::new(Mutex::new(Scene::new(vec![child], None)));

        tree.render(Some(&scene));
        let output = String::from_utf8(terminal.lock().unwrap().take_output_capture()).unwrap();
        assert!(!output.contains(CURSOR_MARKER));
        assert!(output.contains("abcdef"));
        assert!(output.contains("\x1b[1;4H"));
    }
}
