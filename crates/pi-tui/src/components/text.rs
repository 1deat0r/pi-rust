//! Text component — port of `packages/tui/src/components/text.ts`.

use crate::tui::Component;
use crate::utils::{apply_background_to_line, visible_width, wrap_text_with_ansi};

type BackgroundFn = Box<dyn Fn(&str) -> String + Send + Sync>;

pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg: Option<BackgroundFn>,
    cache: std::sync::Mutex<Option<(String, usize, Vec<String>)>>,
}

impl Text {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        bg: Option<BackgroundFn>,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            bg,
            cache: std::sync::Mutex::new(None),
        }
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        *self.cache.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((t, w, lines)) = &*self.cache.lock().unwrap_or_else(|error| error.into_inner())
        {
            if *t == self.text && *w == width {
                return lines.clone();
            }
        }
        if self.text.trim().is_empty() {
            let result = Vec::new();
            *self.cache.lock().unwrap_or_else(|error| error.into_inner()) =
                Some((self.text.clone(), width, result.clone()));
            return result;
        }
        let normalized = self.text.replace('\t', "   ");
        let padding_x = self
            .padding_x
            .min(if width > 1 { (width - 1) / 2 } else { 0 });
        let content_width = (width as isize - (padding_x as isize) * 2).max(1) as usize;
        let wrapped = wrap_text_with_ansi(&normalized, content_width);
        let mut lines: Vec<String> = Vec::new();
        for line in wrapped {
            let line_with_margins =
                format!("{}{}{}", " ".repeat(padding_x), line, " ".repeat(padding_x));
            match &self.bg {
                Some(bg) => lines.push(apply_background_to_line(&line_with_margins, width, &**bg)),
                None => {
                    let visible = visible_width(&line_with_margins);
                    lines.push(format!(
                        "{}{}",
                        line_with_margins,
                        " ".repeat(width.saturating_sub(visible))
                    ));
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
        *self.cache.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((self.text.clone(), width, result.clone()));
        result
    }

    fn invalidate(&mut self) {
        *self.cache.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn renders_content_with_padding() {
        let text = Text::new("hello", 1, 0, None);
        let lines = text.render(10);
        assert!(visible_width(&lines[0]) >= 7);
        assert!(visible_width(&lines[0]) <= 10);
    }

    #[test]
    fn empty_text_renders_nothing() {
        let text = Text::new("   ", 1, 1, None);
        assert_eq!(text.render(10).len(), 0);
    }

    #[test]
    fn caches_empty_text_render_for_repeated_layouts() {
        let text = Text::new("   ", 1, 1, None);
        assert!(text
            .cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none());
        assert!(text.render(10).is_empty());
        let cache = text.cache.lock().unwrap_or_else(|error| error.into_inner());
        let Some((cached_text, cached_width, cached_lines)) = cache.as_ref() else {
            panic!("empty text render should populate the upstream-compatible cache");
        };
        assert_eq!(cached_text, "   ");
        assert_eq!(*cached_width, 10);
        assert!(cached_lines.is_empty());
    }

    #[test]
    fn renders_ansi_footer_with_long_spacing_repeatedly() {
        for cwd in [
            "/tmp/pi-interactive-slash-7ef135b8-38b1-47b1-a918-ff0af7ee5230/project",
            "/tmp/pi-interactive-slash-7c0f88fe-633e-46ee-8377-80b35473f834/project",
        ] {
            let value = format!(
                "\x1b[2m{cwd}\x1b[22m\n\x1b[2m↑1.2k ↓6                                                                           (faux/Faux Model)\x1b[22m"
            );
            let mut text = Text::new(&value, 0, 0, None);
            for _ in 0..1_000 {
                text.set_text(&value);
                let lines = text.render(100);
                assert_eq!(lines.len(), 2);
            }
        }
    }
}
