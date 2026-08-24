//! Component model + differential renderer — port of `packages/tui/src/tui.ts`
//! (the subset the interactive mode uses: component tree, per-line render,
//! input dispatch, diff-based terminal output).

use std::sync::{Arc, Mutex};

use crate::keys::TuiKey;
use crate::terminal::TerminalBackend;
use crate::utils::visible_width;

/// A component renders to lines for a viewport width.
pub trait Component {
    fn render(&self, width: usize) -> Vec<String>;
    fn handle_input(&mut self, _key: &TuiKey) {}
    fn invalidate(&mut self) {}
}

pub type SharedComponent = Arc<Mutex<dyn Component + Send + Sync>>;

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
        let _ = height;
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
    focused: Option<SharedComponent>,
}

impl Tree {
    pub fn new(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        Self {
            terminal,
            last_lines: Vec::new(),
            last_screen_epoch: None,
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
            self.last_lines.clear();
        }
        consumed
    }

    /// Force the next render to redraw every line after the terminal size
    /// changes. Terminals and multiplexers may clear or reposition the
    /// visible screen while delivering a resize event, so a differential
    /// frame based on the old dimensions is not safe to reuse.
    pub fn invalidate(&mut self) {
        self.last_lines.clear();
    }

    pub fn leave_alt_screen(&mut self) {
        let mut term = self.terminal.lock().unwrap();
        let _ = term.leave_raw();
    }

    pub fn focus(&mut self, component: SharedComponent) {
        self.focused = Some(component);
    }

    /// Render the scene, diffing against the previous frame.
    pub fn render(&mut self, scene: Option<&Arc<Mutex<Scene>>>) {
        let (width, height, screen_epoch) = {
            let term = self.terminal.lock().unwrap();
            (term.width(), term.height(), term.screen_epoch())
        };
        if self.last_screen_epoch != Some(screen_epoch) {
            self.last_lines.clear();
            self.last_screen_epoch = Some(screen_epoch);
        }
        let lines: Vec<String> = match scene {
            Some(scene) => {
                let guard = scene.lock().unwrap();
                guard.render(width, height)
            }
            None => vec![" ".repeat(width); height],
        };
        self.diff_render(&lines);
        self.last_lines = lines;
    }

    fn diff_render(&mut self, lines: &[String]) {
        let term = self.terminal.clone();
        let mut t = term.lock().unwrap();
        let common = self.last_lines.len().min(lines.len());
        // Move to the top and rewrite changed lines.
        t.write_raw("\x1b[H");
        for (i, line) in lines.iter().enumerate() {
            let same = i < common && self.last_lines[i] == *line;
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
        t.write_raw(&format!("\x1b[{};1H", lines.len()));
        let _ = &mut self.focused;
    }

    /// Dispatch terminal input to the focused component.
    pub fn dispatch(&mut self, key: &TuiKey) {
        if let Some(focused) = &self.focused {
            let mut guard = focused.lock().unwrap();
            guard.handle_input(key);
        }
    }
}

fn truncate_for_terminal(line: &str, width: usize) -> String {
    if visible_width(line) <= width {
        return line.to_string();
    }
    crate::utils::slice_with_width(line, width)
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
        assert_eq!(visible_width(&lines[0]) >= 5, true);
    }
}
