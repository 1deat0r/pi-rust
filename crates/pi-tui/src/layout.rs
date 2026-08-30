//! Layout nodes and viewport layout — port of `packages/tui/src/layout.ts`
//! and `layout-node.ts`.
//!
//! The original TypeScript implementation keeps layout metadata separate from
//! rendering.  Rust does the same through [`LayoutNode`], which is returned by
//! the optional `Component::layout_node` hook.  Components that do not expose
//! a node continue to use their normal intrinsic rendering path.

use std::sync::Arc;

use crate::tui::{composite_tui_line, SharedComponent, CURSOR_MARKER};
use crate::utils::{slice_by_column, visible_width};

/// Dimensions available to a visibility predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutViewport {
    pub width: usize,
    pub height: usize,
}

/// A stack entry's intrinsic basis. `Auto` asks the child for its natural size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutBasis {
    #[default]
    Auto,
    Cells(usize),
}

/// One child in a vertical or horizontal layout node.
pub struct StackLayoutEntry {
    pub component: SharedComponent,
    pub basis: LayoutBasis,
    pub grow: usize,
    pub shrink: usize,
    pub min_size: usize,
    pub max_size: usize,
    pub visible: Option<Arc<dyn Fn(LayoutViewport) -> bool + Send + Sync>>,
}

impl StackLayoutEntry {
    pub fn new(component: SharedComponent) -> Self {
        Self {
            component,
            basis: LayoutBasis::Auto,
            grow: 0,
            shrink: 1,
            min_size: 0,
            max_size: usize::MAX,
            visible: None,
        }
    }

    pub fn with_basis(mut self, basis: LayoutBasis) -> Self {
        self.basis = basis;
        self
    }

    pub fn with_grow(mut self, grow: usize) -> Self {
        self.grow = grow;
        self
    }

    pub fn with_shrink(mut self, shrink: usize) -> Self {
        self.shrink = shrink;
        self
    }

    pub fn with_min_size(mut self, min_size: usize) -> Self {
        self.min_size = min_size;
        self.max_size = self.max_size.max(min_size);
        self
    }

    pub fn with_max_size(mut self, max_size: usize) -> Self {
        self.max_size = max_size.max(self.min_size);
        self
    }

    pub fn with_visibility(
        mut self,
        visible: impl Fn(LayoutViewport) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.visible = Some(Arc::new(visible));
        self
    }
}

impl Clone for StackLayoutEntry {
    fn clone(&self) -> Self {
        Self {
            component: self.component.clone(),
            basis: self.basis,
            grow: self.grow,
            shrink: self.shrink,
            min_size: self.min_size,
            max_size: self.max_size,
            visible: self.visible.clone(),
        }
    }
}

/// Stack direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutDirection {
    Vertical,
    Horizontal,
}

/// Alignment used by a stack's cross axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAlign {
    Stretch,
    Start,
    Center,
    End,
}

/// A stack node in the layout tree.
#[derive(Clone)]
pub struct StackLayoutNode {
    pub direction: LayoutDirection,
    pub entries: Vec<StackLayoutEntry>,
    pub gap: usize,
    pub align: LayoutAlign,
}

/// Scroll overscroll policy. `Chain` allows wheel/page operations to continue
/// into an ancestor; `Contain` stops at this viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOverscroll {
    Chain,
    Contain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarMode {
    #[default]
    Hidden,
    Auto,
    Always,
}

/// State needed by the layout engine for a scroll node.  The state is shared
/// with the component so layout can reconcile a following-tail viewport after
/// measuring its child without borrowing the component recursively.
pub trait ScrollLayoutState: Send + Sync {
    fn scroll_top(&self) -> usize;
    fn primary(&self) -> bool;
    fn overscroll(&self) -> ScrollOverscroll {
        ScrollOverscroll::Chain
    }
    fn viewport_height(&self) -> usize;
    fn is_following_end(&self) -> bool {
        false
    }
    fn get_content_width(&self, width: usize) -> usize {
        width
    }
    fn update_layout(&self, content_height: usize, viewport_height: usize);
    /// Publish the allocated viewport width before paint-time geometry is
    /// queried. Layout-node rendering does not call the component's ordinary
    /// `render` method, so width must cross this boundary explicitly.
    fn set_viewport_width(&self, _width: usize) {}
    fn scrollbar_visible(&self) -> bool {
        false
    }
    fn scrollbar_style(&self, text: &str) -> String {
        format!("\x1b[100m{text}\x1b[49m")
    }
    fn scroll_by(&self, _lines: isize) -> isize {
        0
    }
    /// Move to an absolute document row. The default keeps custom scroll
    /// states source-compatible by expressing the move through `scroll_by`.
    fn scroll_to(&self, position: usize) {
        let current = self.scroll_top();
        let delta = if position >= current {
            position.saturating_sub(current) as isize
        } else {
            -(current.saturating_sub(position) as isize)
        };
        let _ = self.scroll_by(delta);
    }
    /// Move to an absolute document row while optionally keeping follow-end
    /// disabled. Search uses this to match upstream `scrollTo(...,
    /// {disableFollow: true})`: revealing a match at the tail must not make
    /// subsequent transcript growth jump the viewport away from that match.
    fn scroll_to_with_options(&self, position: usize, _disable_follow: bool) {
        self.scroll_to(position);
    }
    fn scroll_to_start(&self) {}
    fn scroll_to_end(&self) {}
    /// Install the callback used by timer-driven viewport state changes to
    /// request a repaint from the owning event loop.
    fn set_request_render_callback(&self, _callback: Option<Arc<dyn Fn() + Send + Sync>>) {}
}

/// A scroll node containing the child that is laid out as scrollable content.
#[derive(Clone)]
pub struct ScrollLayoutNode {
    pub component: SharedComponent,
    pub state: Arc<dyn ScrollLayoutState>,
}

/// Layout metadata exposed by a component.
#[derive(Clone)]
pub enum LayoutNode {
    Stack(StackLayoutNode),
    Scroll(ScrollLayoutNode),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutConstraint {
    /// Content-sized (min for text, natural for flex).
    Auto,
    /// Fixed cell count.
    Fixed(u32),
    /// Percentage of the parent (0.0..=1.0).
    Percent(f32),
    /// Grow to fill remaining space.
    Grow,
}

impl LayoutConstraint {
    pub fn fixed(n: u32) -> Self {
        Self::Fixed(n)
    }
    pub fn percent(p: f32) -> Self {
        Self::Percent(p)
    }
}

/// Stack direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayout {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HStackLayout {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VStackLayout {
    Top,
    Bottom,
}

/// Solve equal-width flex partitions of `total` for n children with
/// constraints (fixed = their size; percent = fraction; grow = share of
/// remainder). Returns each child's allocated size.
pub fn solve_flex(total: u32, constraints: &[LayoutConstraint]) -> Vec<u32> {
    let mut out = vec![0u32; constraints.len()];
    let mut used = 0u32;
    let mut grows: Vec<usize> = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        match c {
            LayoutConstraint::Fixed(n) => {
                out[i] = *n;
                used += *n;
            }
            LayoutConstraint::Percent(p) => {
                let n = ((total as f32) * p).floor() as u32;
                out[i] = n;
                used += n;
            }
            LayoutConstraint::Auto | LayoutConstraint::Grow => grows.push(i),
        }
    }
    let remaining = total.saturating_sub(used);
    if !grows.is_empty() {
        let each = remaining / grows.len() as u32;
        let mut extra = remaining % grows.len() as u32;
        for idx in grows {
            out[idx] = each
                + if extra > 0 {
                    extra -= 1;
                    1
                } else {
                    0
                };
        }
    }
    out
}

/// Return only stack entries visible for the current viewport.
pub fn visible_stack_entries(
    entries: &[StackLayoutEntry],
    viewport: LayoutViewport,
) -> Vec<StackLayoutEntry> {
    entries
        .iter()
        .filter(|entry| {
            entry
                .visible
                .as_ref()
                .is_none_or(|predicate| predicate(viewport))
        })
        .cloned()
        .collect()
}

fn clamp_size(size: usize, entry: &StackLayoutEntry) -> usize {
    size.max(entry.min_size)
        .min(entry.max_size.max(entry.min_size))
}

fn distribute(sizes: &mut [usize], entries: &[StackLayoutEntry], mut amount: usize, grow: bool) {
    while amount > 0 {
        let candidates: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if grow {
                    (entry.grow > 0 && sizes[index] < entry.max_size).then_some(index)
                } else {
                    (entry.shrink > 0 && sizes[index] > entry.min_size).then_some(index)
                }
            })
            .collect();
        if candidates.is_empty() {
            return;
        }

        let total_weight: usize = candidates
            .iter()
            .map(|&index| {
                let entry = &entries[index];
                if grow {
                    entry.grow
                } else {
                    entry.shrink.saturating_mul(sizes[index].max(1))
                }
            })
            .sum();
        if total_weight == 0 {
            return;
        }

        let mut distributed = 0;
        for index in candidates {
            if amount == 0 {
                break;
            }
            let entry = &entries[index];
            let weight = if grow {
                entry.grow
            } else {
                entry.shrink.saturating_mul(sizes[index].max(1))
            };
            let proposed = (amount.saturating_mul(weight) / total_weight).max(1);
            let capacity = if grow {
                entry.max_size.saturating_sub(sizes[index])
            } else {
                sizes[index].saturating_sub(entry.min_size)
            };
            let delta = proposed.min(capacity).min(amount);
            if delta == 0 {
                continue;
            }
            if grow {
                sizes[index] += delta;
            } else {
                sizes[index] -= delta;
            }
            amount -= delta;
            distributed += delta;
        }
        if distributed == 0 {
            return;
        }
    }
}

/// Allocate stack sizes using upstream basis/grow/shrink/min/max semantics.
pub fn allocate_stack_sizes(
    entries: &[StackLayoutEntry],
    intrinsic_sizes: &[usize],
    available_size: Option<usize>,
    gap: usize,
) -> Vec<usize> {
    let mut sizes: Vec<usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let intrinsic = intrinsic_sizes.get(index).copied().unwrap_or(0);
            clamp_size(
                match entry.basis {
                    LayoutBasis::Auto => intrinsic,
                    LayoutBasis::Cells(value) => value,
                },
                entry,
            )
        })
        .collect();
    let Some(available_size) = available_size else {
        return sizes;
    };
    let content_size = available_size.saturating_sub(entries.len().saturating_sub(1) * gap);
    let total = sizes.iter().sum::<usize>();
    if total < content_size {
        distribute(&mut sizes, entries, content_size - total, true);
    } else if total > content_size {
        distribute(&mut sizes, entries, total - content_size, false);
    }
    sizes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone)]
pub struct LayoutBox {
    pub component: SharedComponent,
    pub rect: LayoutRect,
    /// Signed paint translation retained separately from the public geometry.
    /// Scroll content frequently needs to move above row zero; saturating a
    /// `usize` y-coordinate would silently pin it at the top and render the
    /// wrong transcript rows.
    pub paint_offset_y: isize,
    pub clip: LayoutRect,
    pub children: Vec<LayoutBox>,
    pub parent: Option<usize>,
    pub lines: Option<Vec<String>>,
    pub line_offset: usize,
    pub scroll_view: Option<Arc<dyn ScrollLayoutState>>,
    pub scroll_content_lines: Option<Vec<String>>,
    pub layer: usize,
}

pub struct LayoutFrame {
    pub root: LayoutBox,
    pub width: usize,
    pub height: usize,
    pub lines: Vec<String>,
    pub primary_scroll_view: Option<Arc<dyn ScrollLayoutState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: usize,
    pub track_top: usize,
    pub track_height: usize,
    pub thumb_top: usize,
    pub thumb_height: usize,
    pub max_scroll_top: usize,
}

struct LayoutContext {
    viewport: LayoutViewport,
    render_cache: Vec<(usize, usize, Vec<String>)>,
    primary_scroll_view: Option<Arc<dyn ScrollLayoutState>>,
}

fn component_key(component: &SharedComponent) -> usize {
    Arc::as_ptr(component) as *const () as usize
}

fn render_cached(
    context: &mut LayoutContext,
    component: &SharedComponent,
    width: usize,
) -> Vec<String> {
    let width = width.max(1);
    let key = (component_key(component), width);
    if let Some((_, _, lines)) = context
        .render_cache
        .iter()
        .find(|entry| (entry.0, entry.1) == key)
    {
        return lines.clone();
    }
    let lines = component
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .render(width);
    context.render_cache.push((key.0, key.1, lines.clone()));
    lines
}

fn intersect(a: LayoutRect, b: LayoutRect) -> LayoutRect {
    let right = a.x.saturating_add(a.width).min(b.x.saturating_add(b.width));
    let bottom =
        a.y.saturating_add(a.height)
            .min(b.y.saturating_add(b.height));
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    LayoutRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

fn translate_box(box_: &mut LayoutBox, delta_y: isize) {
    box_.paint_offset_y = box_.paint_offset_y.saturating_add(delta_y);
    for child in &mut box_.children {
        translate_box(child, delta_y);
    }
}

// The recursive layout routine keeps these coordinates and clipping inputs
// explicit at every child boundary; bundling them would obscure the retained
// tree's geometry contract and add a short-lived allocation on each branch.
#[allow(clippy::too_many_arguments)]
fn layout_component(
    context: &mut LayoutContext,
    component: SharedComponent,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    request_render: Option<&Arc<dyn Fn() + Send + Sync>>,
) -> LayoutBox {
    let width = width.max(1);
    let node = component
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .layout_node();
    let Some(node) = node else {
        let lines = render_cached(context, &component, width);
        let allocated_height = height.unwrap_or(lines.len());
        let line_offset = if allocated_height > 0 && lines.len() > allocated_height {
            lines
                .iter()
                .position(|line| line.contains(CURSOR_MARKER))
                .filter(|&row| row >= allocated_height)
                .map_or(0, |row| row - allocated_height + 1)
        } else {
            0
        };
        let rect = LayoutRect {
            x,
            y,
            width,
            height: allocated_height,
        };
        return LayoutBox {
            component,
            rect,
            paint_offset_y: 0,
            clip: intersect(clip, rect),
            children: Vec::new(),
            parent: None,
            lines: Some(lines),
            line_offset,
            scroll_view: None,
            scroll_content_lines: None,
            layer: 0,
        };
    };

    match node {
        LayoutNode::Scroll(scroll) => {
            let previous_top = scroll.state.scroll_top();
            let content_width = scroll.state.get_content_width(width).max(1);
            let initial_content_y = y.saturating_sub(previous_top);
            let child = layout_component(
                context,
                scroll.component.clone(),
                x,
                initial_content_y,
                content_width,
                None,
                // Scroll content is laid out in document coordinates. Defer
                // clipping until paint, after the signed scroll translation,
                // or rows near the document tail get clipped at their old
                // unscrolled y-coordinate.
                LayoutRect {
                    x: 0,
                    y: 0,
                    width: usize::MAX,
                    height: usize::MAX,
                },
                request_render,
            );
            let content_height = child.rect.height;
            let viewport_height = height.unwrap_or(content_height);
            scroll.state.set_viewport_width(width);
            scroll
                .state
                .set_request_render_callback(request_render.cloned());
            scroll.state.update_layout(content_height, viewport_height);
            let actual_top = scroll.state.scroll_top();
            let mut child = child;
            // `LayoutRect.y` is unsigned, so the initial document position is
            // clamped when the viewport is already scrolled past its origin.
            // Translate from that clamped position to the desired signed
            // screen position; using only `previous_top - actual_top` loses
            // the clamped portion after a resize or appended tail content.
            let desired_content_y = y as isize - actual_top as isize;
            let translation = desired_content_y - initial_content_y as isize;
            translate_box(&mut child, translation);
            if scroll.state.primary() || context.primary_scroll_view.is_none() {
                context.primary_scroll_view = Some(scroll.state.clone());
            }
            let rect = LayoutRect {
                x,
                y,
                width,
                height: viewport_height,
            };
            let child_clip = intersect(clip, rect);
            let content_lines = render_cached(context, &scroll.component, content_width);
            LayoutBox {
                component,
                rect,
                paint_offset_y: 0,
                clip: child_clip,
                children: vec![child],
                parent: None,
                lines: None,
                line_offset: 0,
                scroll_view: Some(scroll.state),
                scroll_content_lines: Some(content_lines),
                layer: 0,
            }
        }
        LayoutNode::Stack(stack) => {
            let entries = visible_stack_entries(&stack.entries, context.viewport);
            let gap_total = entries.len().saturating_sub(1) * stack.gap;
            let rect_height;
            let mut children = Vec::with_capacity(entries.len());
            if stack.direction == LayoutDirection::Vertical {
                let intrinsic: Vec<usize> = entries
                    .iter()
                    .map(|entry| match entry.basis {
                        LayoutBasis::Cells(value) => value,
                        LayoutBasis::Auto => render_cached(context, &entry.component, width).len(),
                    })
                    .collect();
                let sizes = allocate_stack_sizes(&entries, &intrinsic, height, stack.gap);
                rect_height = height.unwrap_or_else(|| sizes.iter().sum::<usize>() + gap_total);
                let mut child_y = y;
                let parent_clip = intersect(
                    clip,
                    LayoutRect {
                        x,
                        y,
                        width,
                        height: rect_height,
                    },
                );
                for (entry, size) in entries.iter().zip(sizes.iter().copied()) {
                    children.push(layout_component(
                        context,
                        entry.component.clone(),
                        x,
                        child_y,
                        width,
                        Some(size),
                        parent_clip,
                        request_render,
                    ));
                    child_y = child_y.saturating_add(size).saturating_add(stack.gap);
                }
            } else {
                let intrinsic: Vec<usize> = entries
                    .iter()
                    .map(|entry| match entry.basis {
                        LayoutBasis::Cells(value) => value,
                        LayoutBasis::Auto => render_cached(context, &entry.component, width)
                            .iter()
                            .map(|line| visible_width(line))
                            .max()
                            .unwrap_or(0),
                    })
                    .collect();
                let sizes = allocate_stack_sizes(&entries, &intrinsic, Some(width), stack.gap);
                let child_heights: Vec<usize> = entries
                    .iter()
                    .zip(sizes.iter().copied())
                    .map(|(entry, child_width)| {
                        render_cached(context, &entry.component, child_width.max(1)).len()
                    })
                    .collect();
                rect_height =
                    height.unwrap_or_else(|| child_heights.iter().copied().max().unwrap_or(0));
                let parent_clip = intersect(
                    clip,
                    LayoutRect {
                        x,
                        y,
                        width,
                        height: rect_height,
                    },
                );
                let mut child_x = x;
                for ((entry, child_width), child_height) in
                    entries.iter().zip(sizes).zip(child_heights)
                {
                    let child_height = match stack.align {
                        LayoutAlign::Stretch => rect_height,
                        _ => child_height.min(rect_height),
                    };
                    let child_y = match stack.align {
                        LayoutAlign::Center => y + rect_height.saturating_sub(child_height) / 2,
                        LayoutAlign::End => y + rect_height.saturating_sub(child_height),
                        _ => y,
                    };
                    if child_width > 0 {
                        children.push(layout_component(
                            context,
                            entry.component.clone(),
                            child_x,
                            child_y,
                            child_width,
                            Some(child_height),
                            parent_clip,
                            request_render,
                        ));
                    } else {
                        children.push(LayoutBox {
                            component: entry.component.clone(),
                            rect: LayoutRect {
                                x: child_x,
                                y: child_y,
                                width: 0,
                                height: child_height,
                            },
                            paint_offset_y: 0,
                            clip: LayoutRect {
                                x: child_x,
                                y: child_y,
                                width: 0,
                                height: 0,
                            },
                            children: Vec::new(),
                            parent: None,
                            lines: None,
                            line_offset: 0,
                            scroll_view: None,
                            scroll_content_lines: None,
                            layer: 0,
                        });
                    }
                    child_x = child_x
                        .saturating_add(child_width)
                        .saturating_add(stack.gap);
                }
            }
            let rect = LayoutRect {
                x,
                y,
                width,
                height: rect_height,
            };
            LayoutBox {
                component,
                rect,
                paint_offset_y: 0,
                clip: intersect(clip, rect),
                children,
                parent: None,
                lines: None,
                line_offset: 0,
                scroll_view: None,
                scroll_content_lines: None,
                layer: 0,
            }
        }
    }
}

fn paint_box(box_: &LayoutBox, screen: &mut [String], width: usize) {
    paint_box_with_clip(box_, screen, width, None);
}

fn paint_box_with_clip(
    box_: &LayoutBox,
    screen: &mut [String],
    width: usize,
    inherited_clip: Option<LayoutRect>,
) {
    // A scroll node supplies the viewport clip to its content. Content boxes
    // are laid out in document coordinates, so their own pre-scroll clips
    // must not be allowed to discard rows before the signed paint translation
    // is applied.
    let translated_clip = LayoutRect {
        x: box_.clip.x,
        y: box_.clip.y.saturating_add_signed(box_.paint_offset_y),
        width: box_.clip.width,
        height: box_.clip.height,
    };
    let clip = inherited_clip
        .map(|parent| intersect(parent, translated_clip))
        .unwrap_or(translated_clip);
    if let Some(lines) = &box_.lines {
        let paint_y = box_.rect.y as isize + box_.paint_offset_y;
        let clip_top = clip.y as isize;
        let clip_bottom = clip.y.saturating_add(clip.height) as isize;
        let first = paint_y.max(clip_top).max(0) as usize;
        let last = (paint_y + box_.rect.height as isize)
            .min(clip_bottom)
            .min(screen.len() as isize)
            .max(first as isize) as usize;
        for (row, target) in screen.iter_mut().enumerate().take(last).skip(first) {
            let source = box_.line_offset as isize + row as isize - paint_y;
            if source < 0 {
                continue;
            }
            let source = source as usize;
            let Some(line) = lines.get(source) else {
                continue;
            };
            *target = if box_.rect.x == 0 && box_.rect.width >= width && target.is_empty() {
                line.clone()
            } else {
                composite_tui_line(target, line, box_.rect.x, box_.rect.width, width)
            };
        }
    }
    for child in &box_.children {
        paint_box_with_clip(child, screen, width, Some(clip));
    }
    paint_scrollbar(box_, screen, width, clip);
}

fn style_scrollbar_cell(line: &str, column: usize, width: usize, styled: &str) -> String {
    composite_tui_line(line, styled, column, 1, width)
}

fn paint_scrollbar(box_: &LayoutBox, screen: &mut [String], width: usize, clip: LayoutRect) {
    let Some(geometry) = get_scrollbar_geometry(box_) else {
        return;
    };
    let Some(state) = &box_.scroll_view else {
        return;
    };
    let styled = state.scrollbar_style("█");
    let translated_thumb_top = geometry.thumb_top as isize + box_.paint_offset_y;
    for row in translated_thumb_top.max(0) as usize
        ..(translated_thumb_top + geometry.thumb_height as isize).max(0) as usize
    {
        if row < clip.y || row >= clip.y.saturating_add(clip.height) || row >= screen.len() {
            continue;
        }
        screen[row] = style_scrollbar_cell(&screen[row], geometry.column, width, &styled);
    }
}

/// Render a component tree into a clipped viewport and retain its geometry.
pub fn render_layout_frame(root: SharedComponent, width: usize, height: usize) -> LayoutFrame {
    render_layout_frame_with_request(root, width, height, None)
}

/// Render a component tree while wiring scroll nodes to an owner-provided
/// repaint callback. A callback is optional so callers that drive rendering
/// synchronously can keep using [`render_layout_frame`].
pub fn render_layout_frame_with_request(
    root: SharedComponent,
    width: usize,
    height: usize,
    request_render: Option<Arc<dyn Fn() + Send + Sync>>,
) -> LayoutFrame {
    let width = width.max(1);
    let height = height.max(1);
    let viewport = LayoutViewport { width, height };
    let mut context = LayoutContext {
        viewport,
        render_cache: Vec::new(),
        primary_scroll_view: None,
    };
    let root_box = layout_component(
        &mut context,
        root,
        0,
        0,
        width,
        Some(height),
        LayoutRect {
            x: 0,
            y: 0,
            width,
            height,
        },
        request_render.as_ref(),
    );
    let mut lines = vec![String::new(); height];
    paint_box(&root_box, &mut lines, width);
    LayoutFrame {
        root: root_box,
        width,
        height,
        lines,
        primary_scroll_view: context.primary_scroll_view,
    }
}

/// Find a scroll node by shared state identity.
pub fn get_scroll_view_box(
    frame: &LayoutFrame,
    target: &Arc<dyn ScrollLayoutState>,
) -> Option<LayoutRect> {
    fn visit(box_: &LayoutBox, target: &Arc<dyn ScrollLayoutState>) -> Option<LayoutRect> {
        if box_
            .scroll_view
            .as_ref()
            .is_some_and(|state| Arc::ptr_eq(state, target))
        {
            return Some(box_.rect);
        }
        box_.children.iter().find_map(|child| visit(child, target))
    }
    visit(&frame.root, target)
}

/// Return scroll nodes whose clipped rectangles contain a point, deepest first.
pub fn get_scroll_views_at(
    frame: &LayoutFrame,
    x: usize,
    y: usize,
) -> Vec<Arc<dyn ScrollLayoutState>> {
    fn visit(
        box_: &LayoutBox,
        x: usize,
        y: usize,
        depth: usize,
        out: &mut Vec<(usize, Arc<dyn ScrollLayoutState>)>,
    ) {
        let inside = |rect: LayoutRect| {
            x >= rect.x
                && y >= rect.y
                && x < rect.x.saturating_add(rect.width)
                && y < rect.y.saturating_add(rect.height)
        };
        if !inside(box_.clip) {
            return;
        }
        if let Some(state) = &box_.scroll_view {
            if inside(box_.rect) {
                out.push((depth, state.clone()));
            }
        }
        for child in &box_.children {
            visit(child, x, y, depth + 1, out);
        }
    }
    let mut found = Vec::new();
    visit(&frame.root, x, y, 0, &mut found);
    found.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    found.into_iter().map(|(_, state)| state).collect()
}

/// Return the visible scrollbar geometry for a scroll layout box.
pub fn get_scrollbar_geometry(box_: &LayoutBox) -> Option<ScrollbarGeometry> {
    let state = box_.scroll_view.as_ref()?;
    if !state.scrollbar_visible() || box_.rect.width == 0 || box_.rect.height == 0 {
        return None;
    }
    let column = box_.rect.x + box_.rect.width - 1;
    if column < box_.clip.x || column >= box_.clip.x.saturating_add(box_.clip.width) {
        return None;
    }
    let content_height = box_
        .children
        .first()
        .map(|child| child.rect.height)
        .or_else(|| box_.scroll_content_lines.as_ref().map(Vec::len))
        .unwrap_or(0);
    let track_height = box_.rect.height;
    let min_thumb_height = 2.min(track_height);
    let thumb_height = if content_height == 0 {
        track_height
    } else {
        track_height
            .saturating_mul(track_height)
            .saturating_add(content_height / 2)
            .checked_div(content_height)
            .unwrap_or(0)
            .max(min_thumb_height)
            .min(track_height)
    };
    let max_scroll_top = content_height.saturating_sub(track_height);
    let max_thumb_top = track_height.saturating_sub(thumb_height);
    let thumb_offset = if max_scroll_top == 0 {
        0
    } else {
        (state.scroll_top().min(max_scroll_top) * max_thumb_top)
            .checked_div(max_scroll_top)
            .unwrap_or(0)
    };
    Some(ScrollbarGeometry {
        column,
        track_top: box_.rect.y,
        track_height,
        thumb_top: box_.rect.y + thumb_offset,
        thumb_height,
        max_scroll_top,
    })
}

/// Clamp a rendered line to a terminal width without splitting a grapheme.
pub fn clamp_layout_line(line: &str, width: usize) -> String {
    if visible_width(line) <= width {
        line.to_string()
    } else {
        slice_by_column(line, 0, width)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::Component;

    #[test]
    fn fixed_and_grow_split() {
        let sizes = solve_flex(
            100,
            &[
                LayoutConstraint::Fixed(20),
                LayoutConstraint::Grow,
                LayoutConstraint::Grow,
            ],
        );
        assert_eq!(sizes, vec![20, 40, 40]);
    }

    #[test]
    fn percent_then_grow() {
        let sizes = solve_flex(
            100,
            &[LayoutConstraint::Percent(0.5), LayoutConstraint::Grow],
        );
        assert_eq!(sizes, vec![50, 50]);
    }

    #[test]
    fn all_fixed_no_remainder() {
        let sizes = solve_flex(
            40,
            &[LayoutConstraint::Fixed(10), LayoutConstraint::Fixed(30)],
        );
        assert_eq!(sizes, vec![10, 30]);
        // grow children get zero when nothing remains.
        let sizes = solve_flex(40, &[LayoutConstraint::Fixed(50)]);
        assert_eq!(sizes, vec![50]);
    }

    #[test]
    fn horizontal_auto_basis_uses_the_widest_multiline_row() {
        struct NaturalLines(Vec<String>);

        impl Component for NaturalLines {
            fn render(&self, _width: usize) -> Vec<String> {
                self.0.clone()
            }
        }

        struct HorizontalNode {
            entries: Vec<StackLayoutEntry>,
        }

        impl Component for HorizontalNode {
            fn render(&self, _width: usize) -> Vec<String> {
                Vec::new()
            }

            fn layout_node(&self) -> Option<LayoutNode> {
                Some(LayoutNode::Stack(StackLayoutNode {
                    direction: LayoutDirection::Horizontal,
                    entries: self.entries.clone(),
                    gap: 0,
                    align: LayoutAlign::Stretch,
                }))
            }
        }

        let multiline: SharedComponent = Arc::new(std::sync::Mutex::new(NaturalLines(vec![
            "short".to_string(),
            "123456".to_string(),
        ])));
        let sibling: SharedComponent =
            Arc::new(std::sync::Mutex::new(NaturalLines(vec!["b".to_string()])));
        let root: SharedComponent = Arc::new(std::sync::Mutex::new(HorizontalNode {
            entries: vec![
                StackLayoutEntry::new(multiline),
                StackLayoutEntry::new(sibling),
            ],
        }));

        let frame = render_layout_frame(root, 10, 2);
        assert_eq!(
            frame
                .root
                .children
                .iter()
                .map(|child| child.rect.width)
                .collect::<Vec<_>>(),
            vec![6, 1]
        );
        assert!(frame.lines[0].contains('b'));
    }
}
