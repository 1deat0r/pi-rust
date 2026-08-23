//! Input component — a single-line text editor with editing keys.

use crate::keys::TuiKey;
use crate::tui::Component;
use crate::utils::slice_with_width;

pub struct Input {
    pub value: String,
    pub cursor: usize, // byte offset
    pub prompt: String,
}

impl Input {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            prompt: prompt.into(),
        }
    }
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
    }
    fn char_index_before(&self) -> usize {
        self.value[..self.cursor].chars().count()
    }
    fn insert_char(&mut self, c: char) {
        let ci = self.char_index_before();
        let mut chars: Vec<char> = self.value.chars().collect();
        chars.insert(ci, c);
        self.value = chars.into_iter().collect();
        // Move the cursor past the inserted character (byte offset of char ci
        // plus the char's width).
        let offset = self
            .value
            .char_indices()
            .nth(ci)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len());
        self.cursor = offset + c.len_utf8();
    }
}

impl Component for Input {
    fn render(&self, width: usize) -> Vec<String> {
        let prompt_width = crate::utils::visible_width(&self.prompt);
        let avail = width.saturating_sub(prompt_width);
        let visible = slice_with_width(&self.value, avail);
        vec![format!("{}{}", self.prompt, visible)]
    }

    fn handle_input(&mut self, key: &TuiKey) {
        match key.base.as_str() {
            "backspace" => {
                if self.cursor > 0 {
                    let ci = self.char_index_before();
                    let mut chars: Vec<char> = self.value.chars().collect();
                    if ci > 0 {
                        chars.remove(ci - 1);
                        self.value = chars.into_iter().collect();
                        self.cursor = self
                            .value
                            .char_indices()
                            .nth(ci - 1)
                            .map(|(i, _)| i)
                            .unwrap_or(self.value.len());
                    }
                }
            }
            "delete" => {
                let ci = self.char_index_before();
                let mut chars: Vec<char> = self.value.chars().collect();
                if ci < chars.len() {
                    chars.remove(ci);
                    self.value = chars.into_iter().collect();
                    self.cursor = self
                        .value
                        .char_indices()
                        .nth(ci)
                        .map(|(i, _)| i)
                        .unwrap_or(self.value.len());
                }
            }
            "left" => {
                if self.cursor > 0 {
                    let prefix = &self.value[..self.cursor];
                    self.cursor = prefix
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            "right" => {
                if self.cursor < self.value.len() {
                    if let Some(c) = self.value[self.cursor..].chars().next() {
                        self.cursor += c.len_utf8();
                    }
                }
            }
            "home" => self.cursor = 0,
            "end" => self.cursor = self.value.len(),
            _ => {
                if key.ctrl || key.alt {
                    return;
                }
                if let Some(c) = key.base.chars().next() {
                    if c.is_control() {
                        return;
                    }
                    self.insert_char(c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_inserts_characters() {
        let mut input = Input::new("> ");
        input.handle_input(&TuiKey::simple("h"));
        input.handle_input(&TuiKey::simple("i"));
        assert_eq!(input.value, "hi");
        assert_eq!(input.cursor, 2);
    }

    #[test]
    fn backspace_removes() {
        let mut input = Input::new("> ");
        for c in ["h", "i"] {
            input.handle_input(&TuiKey::simple(c));
        }
        input.handle_input(&TuiKey::ctrl("h"));
        input.handle_input(&TuiKey::simple(""));
        input.value = "h".to_string();
        input.cursor = 1;
        input.handle_input(&TuiKey::ctrl("h"));
        input.handle_input(&TuiKey::simple(""));
        // "h" state remains; go back to "i" scenario: ctrl+h is not backspace in this model.
        input.value = "hi".to_string();
        input.cursor = 2;
        input.handle_input(&TuiKey::simple("backspace"));
        assert_eq!(input.value, "h");
    }

    #[test]
    fn unicode_cursor_editing() {
        let mut input = Input::new("> ");
        for c in ["h", "é", "i"] {
            input.handle_input(&TuiKey::simple(c));
        }
        assert_eq!(input.value, "héi");
        assert_eq!(input.cursor, 4);
        input.handle_input(&TuiKey::simple("left"));
        assert_eq!(input.cursor, 3);
        input.handle_input(&TuiKey::simple("left"));
        assert_eq!(input.cursor, 1);
    }
}
