//! VStack/HStack — port of `packages/tui/src/components/stack.ts`.

use crate::layout::{
    LayoutAlign, LayoutBasis, LayoutDirection, LayoutNode, StackLayoutEntry, StackLayoutNode,
};
use crate::tui::{Component, SharedComponent};

/// Vertical stack: children render full-width and stack rows.
pub struct VStack {
    pub children: Vec<SharedComponent>,
    layout_entries: Option<Vec<StackLayoutEntry>>,
}

impl VStack {
    pub fn new(children: Vec<SharedComponent>) -> Self {
        Self {
            children,
            layout_entries: None,
        }
    }

    /// Construct a vertical stack with the same basis/grow/shrink/minimum
    /// metadata that upstream Pi supplies to its layout engine.
    pub fn with_layout_entries(entries: Vec<StackLayoutEntry>) -> Self {
        let children = entries
            .iter()
            .map(|entry| entry.component.clone())
            .collect();
        Self {
            children,
            layout_entries: Some(entries),
        }
    }
}

impl Component for VStack {
    fn render(&self, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        for child in &self.children {
            out.extend(
                child
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .render(width),
            );
        }
        out
    }
    fn invalidate(&mut self) {
        for child in &self.children {
            child
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate();
        }
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Stack(StackLayoutNode {
            direction: LayoutDirection::Vertical,
            entries: self.layout_entries.clone().unwrap_or_else(|| {
                self.children
                    .iter()
                    .cloned()
                    .map(StackLayoutEntry::new)
                    .collect()
            }),
            gap: 0,
            align: LayoutAlign::Stretch,
        }))
    }
}

/// Horizontal stack: children share the width via flex constraints.
pub struct HStack {
    pub children: Vec<(f32, SharedComponent)>, // (weight, child); weight <= 0 => grow
}

impl HStack {
    pub fn new(children: Vec<(f32, SharedComponent)>) -> Self {
        Self { children }
    }
}

impl Component for HStack {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut fixed_widths = 0usize;
        let mut grow_count = 0usize;
        for (w, _) in &self.children {
            if *w > 0.0 {
                fixed_widths += *w as usize;
            } else {
                grow_count += 1;
            }
        }
        let available = width.saturating_sub(fixed_widths);
        let per = available.checked_div(grow_count).unwrap_or(0);
        let mut parts: Vec<Vec<String>> = Vec::new();
        let mut widths: Vec<usize> = Vec::new();
        for (w, child) in &self.children {
            let child_width = if *w > 0.0 { *w as usize } else { per };
            parts.push(if child_width == 0 {
                Vec::new()
            } else {
                child
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .render(child_width)
            });
            widths.push(child_width);
        }
        let height = parts.iter().map(|p| p.len()).max().unwrap_or(0);
        let mut lines = Vec::with_capacity(height);
        for row in 0..height {
            let mut line = String::new();
            for (i, part) in parts.iter().enumerate() {
                let part_line = part.get(row).cloned().unwrap_or_default();
                let plen = crate::utils::visible_width(&part_line);
                line.push_str(&part_line);
                if plen < widths[i] {
                    line.push_str(&" ".repeat(widths[i] - plen));
                }
            }
            if crate::utils::visible_width(&line) < width {
                line.push_str(&" ".repeat(width - crate::utils::visible_width(&line)));
            }
            lines.push(line);
        }
        lines
    }

    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Stack(StackLayoutNode {
            direction: LayoutDirection::Horizontal,
            entries: self
                .children
                .iter()
                .map(|(weight, component)| {
                    let entry = StackLayoutEntry::new(component.clone());
                    if *weight > 0.0 {
                        entry.with_basis(LayoutBasis::Cells((*weight).floor() as usize))
                    } else {
                        entry.with_grow(1)
                    }
                })
                .collect(),
            gap: 0,
            align: LayoutAlign::Stretch,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::components::Text;
    use crate::utils::visible_width;
    use std::sync::{Arc, Mutex};

    #[test]
    fn zero_width_hstack_does_not_render_zero_width_children() {
        let left: SharedComponent = Arc::new(Mutex::new(Text::new("left", 0, 0, None)));
        let right: SharedComponent = Arc::new(Mutex::new(Text::new("right", 0, 0, None)));
        let stack = HStack::new(vec![(0.0, left), (0.0, right)]);

        assert!(stack.render(0).is_empty());
        assert!(stack.render(1).iter().all(|line| visible_width(line) <= 1));
    }
}
