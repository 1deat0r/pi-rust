//! VStack/HStack — port of `packages/tui/src/components/stack.ts`.

use crate::tui::{Component, SharedComponent};

/// Vertical stack: children render full-width and stack rows.
pub struct VStack {
    pub children: Vec<SharedComponent>,
}

impl VStack {
    pub fn new(children: Vec<SharedComponent>) -> Self {
        Self { children }
    }
}

impl Component for VStack {
    fn render(&self, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        for child in &self.children {
            out.extend(child.lock().unwrap().render(width));
        }
        out
    }
    fn invalidate(&mut self) {
        for child in &self.children {
            child.lock().unwrap().invalidate();
        }
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
        let per = if grow_count > 0 {
            available / grow_count
        } else {
            0
        };
        let mut parts: Vec<Vec<String>> = Vec::new();
        let mut widths: Vec<usize> = Vec::new();
        for (w, child) in &self.children {
            let child_width = if *w > 0.0 { *w as usize } else { per };
            parts.push(child.lock().unwrap().render(child_width));
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
}
