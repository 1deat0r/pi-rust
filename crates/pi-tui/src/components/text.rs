//! Text component — port of `packages/tui/src/components/text.ts`.

use crate::utils::{apply_background_to_line, visible_width, wrap_text_with_ansi};
use crate::tui::Component;

pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg: Option<Box<dyn Fn(&str) -> String + Send + Sync>>,
    cache: std::sync::Mutex<Option<(String, usize, Vec<String>)>>,
}

impl Text {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        bg: Option<Box<dyn Fn(&str) -> String + Send + Sync>>,
    ) -> Self {
        Self { text: text.into(), padding_x, padding_y, bg, cache: std::sync::Mutex::new(None) }
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        *self.cache.lock().unwrap() = None;
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((t, w, lines)) = &*self.cache.lock().unwrap() {
            if *t == self.text && *w == width {
                return lines.clone();
            }
        }
        if self.text.trim().is_empty() {
            return Vec::new();
        }
        let normalized = self.text.replace('\t', "   ");
        let padding_x = self.padding_x.min(if width > 1 { (width - 1) / 2 } else { 0 });
        let content_width = (width as isize - (padding_x as isize) * 2).max(1) as usize;
        let wrapped = wrap_text_with_ansi(&normalized, content_width);
        let mut lines: Vec<String> = Vec::new();
        for line in wrapped {
            let line_with_margins = format!("{}{}{}", " ".repeat(padding_x), line, " ".repeat(padding_x));
            match &self.bg {
                Some(bg) => lines.push(apply_background_to_line(&line_with_margins, width, &**bg)),
                None => {
                    let visible = visible_width(&line_with_margins);
                    lines.push(format!("{}{}", line_with_margins, " ".repeat(width.saturating_sub(visible))));
                }
            }
        }
        let empty = " ".repeat(width);
        let mut result = Vec::new();
        for _ in 0..self.padding_y {
            if let Some(bg) = &self.bg {
                result.push(apply_background_to_line(&empty, width, &**bg));
            } else {
                result.push(empty.clone());
            }
        }
        result.extend(lines);
        for _ in 0..self.padding_y {
            if let Some(bg) = &self.bg {
                result.push(apply_background_to_line(&empty, width, &**bg));
            } else {
                result.push(empty.clone());
            }
        }
        if result.is_empty() {
            result.push(empty);
        }
        *self.cache.lock().unwrap() = Some((self.text.clone(), width, result.clone()));
        result
    }

    fn invalidate(&mut self) {
        *self.cache.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_content_with_padding() {
        let text = Text::new("hello", 1, 0, None);
        let lines = text.render(10);
        assert_eq!(visible_width(&lines[0]) >= 7, true);
        assert_eq!(visible_width(&lines[0]) <= 10, true);
    }

    #[test]
    fn empty_text_renders_nothing() {
        let text = Text::new("   ", 1, 1, None);
        assert_eq!(text.render(10).len(), 0);
    }
}
