//! SelectList component — a vertical list with a highlighted selection.

use crate::keys::TuiKey;
use crate::tui::Component;

#[cfg(test)]
use crate::utils::strip_ansi_codes;

pub struct SelectList {
    pub options: Vec<String>,
    pub selected: usize,
    visible_count: usize,
    highlight: Box<dyn Fn(&str) -> String + Send + Sync>,
}

impl SelectList {
    pub fn new(options: Vec<String>, selected: usize) -> Self {
        Self {
            options,
            selected,
            visible_count: 10,
            highlight: Box::new(|line| format!("\x1b[7m{line}\x1b[0m")),
        }
    }
}

impl Component for SelectList {
    fn render(&self, width: usize) -> Vec<String> {
        let _ = width;
        let total = self.options.len();
        if total == 0 {
            return Vec::new();
        }
        let start = self.selected.saturating_sub(self.visible_count.saturating_sub(1));
        let start = start.min(total.saturating_sub(1));
        let end = (start + self.visible_count).min(total);
        let mut lines = Vec::new();
        for (i, option) in self.options[start..end].iter().enumerate() {
            let is_selected = i + start == self.selected;
            let marker = if is_selected { "›" } else { " " };
            let line = format!("{marker} {option}");
            if is_selected {
                lines.push((self.highlight)(&line));
            } else {
                lines.push(line);
            }
        }
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        match key.base.as_str() {
            "up" => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            "down" => {
                if self.selected + 1 < self.options.len() {
                    self.selected += 1;
                }
            }
            "home" => self.selected = 0,
            "end" => self.selected = self.options.len().saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_moves_and_renders_highlight() {
        let mut list = SelectList::new(vec!["a".into(), "b".into(), "c".into()], 0);
        list.handle_input(&TuiKey::simple("down"));
        assert_eq!(list.selected, 1);
        let lines = list.render(40);
        assert!(strip_ansi_codes(&lines[1]).contains('›'));
        assert!(lines[1].contains("\x1b[7m"));
    }
}
