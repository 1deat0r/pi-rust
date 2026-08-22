//! SelectList component — port of `packages/tui/src/components/select-list.ts`.
//!
//! A vertical list of items (value/label/description) with a highlighted
//! selection, description column layout, and scroll indicators.

use crate::keys::TuiKey;
use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width};

#[cfg(test)]
use crate::utils::strip_ansi_codes;

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

fn normalize_to_single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

/// A selectable item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl SelectItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>, description: Option<String>) -> Self {
        Self { value: value.into(), label: label.into(), description }
    }
}

/// Theme functions for the list.
pub struct SelectListTheme {
    pub selected_prefix: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub selected_text: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub description: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub scroll_info: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub no_match: Box<dyn Fn(&str) -> String + Send + Sync>,
}

impl std::fmt::Debug for SelectListTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectListTheme").finish()
    }
}

/// A plain theme (identity functions) for tests and simple use.
pub fn plain_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(|s| s.to_string()),
        selected_text: Box::new(|s| s.to_string()),
        description: Box::new(|s| s.to_string()),
        scroll_info: Box::new(|s| s.to_string()),
        no_match: Box::new(|s| s.to_string()),
    }
}

/// Layout options for the description column.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
}

pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_items: Vec<SelectItem>,
    selected_index: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayoutOptions,
}

impl SelectList {
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayoutOptions,
    ) -> Self {
        Self {
            filtered_items: items.clone(),
            items,
            selected_index: 0,
            max_visible,
            theme,
            layout,
        }
    }

    pub fn set_filter(&mut self, filter: &str) {
        let lower = filter.to_lowercase();
        self.filtered_items = self
            .items
            .iter()
            .filter(|item| item.value.to_lowercase().starts_with(&lower))
            .cloned()
            .collect();
        self.selected_index = 0;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        let max = self.filtered_items.len().saturating_sub(1);
        self.selected_index = clamp(index, 0, max);
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn get_selected_item(&self) -> Option<&SelectItem> {
        self.filtered_items.get(self.selected_index)
    }

    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.filtered_items = self.items.clone();
        self.selected_index = 0;
    }

    fn get_primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        (
            std::cmp::max(1, std::cmp::min(raw_min, raw_max)),
            std::cmp::max(1, std::cmp::max(raw_min, raw_max)),
        )
    }

    fn get_primary_column_width(&self) -> usize {
        let (min, max) = self.get_primary_column_bounds();
        let widest = self
            .filtered_items
            .iter()
            .map(|item| visible_width(&Self::get_display_value(item)) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        clamp(widest, min, max)
    }

    fn get_display_value(item: &SelectItem) -> String {
        if !item.label.is_empty() {
            item.label.clone()
        } else {
            item.value.clone()
        }
    }

    fn truncate_primary(&self, item: &SelectItem, max_width: usize) -> String {
        let display_value = Self::get_display_value(item);
        truncate_to_width(&display_value, max_width, "")
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description_single_line: Option<&str>,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(description) = description_single_line {
            if width > 40 {
                let effective_primary_column_width =
                    std::cmp::max(1, std::cmp::min(primary_column_width, width.saturating_sub(prefix_width + 4)));
                let max_primary_width = std::cmp::max(1, effective_primary_column_width.saturating_sub(PRIMARY_COLUMN_GAP));
                let truncated_value = self.truncate_primary(item, max_primary_width);
                let truncated_value_width = visible_width(&truncated_value);
                let spacing = " ".repeat(std::cmp::max(1, effective_primary_column_width.saturating_sub(truncated_value_width)));
                let description_start = prefix_width + truncated_value_width + spacing.len();
                let remaining_width = width.saturating_sub(description_start + 2);

                if remaining_width > MIN_DESCRIPTION_WIDTH {
                    let truncated_desc = truncate_to_width(description, remaining_width, "");
                    if is_selected {
                        return (self.theme.selected_text)(&format!("{prefix}{truncated_value}{spacing}{truncated_desc}"));
                    }
                    let desc_text = (self.theme.description)(&format!("{spacing}{truncated_desc}"));
                    return format!("{prefix}{truncated_value}{desc_text}");
                }
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated_value = self.truncate_primary(item, max_width);
        if is_selected {
            (self.theme.selected_text)(&format!("{prefix}{truncated_value}"))
        } else {
            format!("{prefix}{truncated_value}")
        }
    }
}

impl Component for SelectList {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        if self.filtered_items.is_empty() {
            lines.push((self.theme.no_match)("  No matching commands"));
            return lines;
        }

        let primary_column_width = self.get_primary_column_width();

        let start_index = std::cmp::max(
            0,
            std::cmp::min(
                self.selected_index.saturating_sub(self.max_visible / 2),
                self.filtered_items.len().saturating_sub(self.max_visible),
            ),
        );
        let end_index = std::cmp::min(start_index + self.max_visible, self.filtered_items.len());

        for i in start_index..end_index {
            let item = &self.filtered_items[i];
            let is_selected = i == self.selected_index;
            let description_single_line = item.description.as_deref().map(normalize_to_single_line);
            lines.push(self.render_item(
                item,
                is_selected,
                width,
                description_single_line.as_deref(),
                primary_column_width,
            ));
        }

        if start_index > 0 || end_index < self.filtered_items.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, self.filtered_items.len());
            lines.push((self.theme.scroll_info)(&truncate_to_width(&scroll_text, width.saturating_sub(2), "")));
        }

        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        match key.base.as_str() {
            "up" => {
                let len = self.filtered_items.len();
                if len > 0 {
                    self.selected_index = if self.selected_index == 0 { len - 1 } else { self.selected_index - 1 };
                }
            }
            "down" => {
                let len = self.filtered_items.len();
                if len > 0 {
                    self.selected_index = if self.selected_index == len - 1 { 0 } else { self.selected_index + 1 };
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: &str, description: Option<&str>) -> SelectItem {
        SelectItem::new(value.to_string(), value.to_string(), description.map(|s| s.to_string()))
    }

    fn rendered_first(list: &SelectList, width: usize) -> String {
        let lines = list.render(width);
        assert!(!lines.is_empty());
        lines[0].clone()
    }

    fn visible_index_of(line: &str, text: &str) -> usize {
        let idx = line.find(text).expect("text present");
        visible_width(&line[..idx])
    }

    #[test]
    fn normalizes_multiline_descriptions() {
        let items = vec![item("test", Some("Line one\nLine two\nLine three"))];
        let list = SelectList::new(items, 5, plain_theme(), SelectListLayoutOptions::default());
        let rendered = list.render(100);
        assert!(rendered.len() > 0);
        assert!(!rendered[0].contains('\n'));
        assert!(rendered[0].contains("Line one Line two Line three"));
    }

    #[test]
    fn keeps_descriptions_aligned_when_truncated() {
        let items = vec![
            item("short", Some("short description")),
            item("very-long-command-name-that-needs-truncation", Some("long description")),
        ];
        let list = SelectList::new(items, 5, plain_theme(), SelectListLayoutOptions::default());
        let rendered = list.render(80);
        let a = visible_index_of(&rendered[0], "short description");
        let b = visible_index_of(&rendered[1], "long description");
        assert_eq!(a, b);
    }

    #[test]
    fn uses_configured_minimum_primary_column_width() {
        let items = vec![item("a", Some("first")), item("bb", Some("second"))];
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(20),
        };
        let list = SelectList::new(items, 5, plain_theme(), layout);
        let rendered = list.render(80);
        let char_idx = |line: &str, text: &str| line.find(text).map(|b| line[..b].chars().count());
        assert_eq!(char_idx(&rendered[0], "first"), Some(14));
        assert_eq!(char_idx(&rendered[1], "second"), Some(14));
    }

    #[test]
    fn uses_configured_maximum_primary_column_width() {
        let items = vec![
            item("very-long-command-name-that-needs-truncation", Some("first")),
            item("short", Some("second")),
        ];
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(20),
        };
        let list = SelectList::new(items, 5, plain_theme(), layout);
        let rendered = list.render(80);
        assert_eq!(visible_index_of(&rendered[0], "first"), 22);
        assert_eq!(visible_index_of(&rendered[1], "second"), 22);
    }

    #[test]
    fn selection_wraps_and_scrolls() {
        let items = vec![item("a", None), item("b", None), item("c", None)];
        let mut list = SelectList::new(items, 5, plain_theme(), SelectListLayoutOptions::default());
        list.handle_input(&TuiKey::simple("up"));
        // Empty-item guard; list has 3 so up from 0 wraps to 2.
        assert_eq!(list.selected_index(), 2);
        list.handle_input(&TuiKey::simple("down"));
        assert_eq!(list.selected_index(), 0);
    }

    #[test]
    fn renders_scroll_indicator() {
        let items: Vec<SelectItem> = (0..3)
            .map(|i| SelectItem::new(format!("item-{i}"), format!("item-{i}"), None))
            .collect();
        let mut list = SelectList::new(items, 1, plain_theme(), SelectListLayoutOptions::default());
        list.set_selected_index(2);
        let rendered = list.render(80);
        assert!(strip_ansi_codes(&rendered.last().unwrap().to_string()).contains("3/3"));
    }
}

