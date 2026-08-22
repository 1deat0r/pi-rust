//! SettingsList component — port of `packages/tui/src/components/settings-list.ts`.
//!
//! A settings screen: selectable items with a current value, cycling on
//! Enter/Space, optional submenu components, and optional search filtering.

use crate::fuzzy::fuzzy_filter;
use crate::keys::TuiKey;
use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};
use crate::components::input::Input;

/// A submenu closure: (current value, expanded) -> optional component.
pub type SubmenuFn = Box<dyn Fn(&str, bool) -> Option<Box<dyn Component + Send + Sync>> + Send + Sync>;
/// A two-argument style function (text, selected).
pub type LabelStyleFn = Box<dyn Fn(&str, bool) -> String + Send + Sync>;

/// A single setting item.
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    /// If provided, Enter/Space cycles through these values.
    pub values: Option<Vec<String>>,
    /// If provided, Enter opens this submenu (returns a component).
    pub submenu: Option<SubmenuFn>,
}

impl std::fmt::Debug for SettingItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingItem").field("id", &self.id).field("label", &self.label).finish()
    }
}

/// A minimal settings item for simple cycling lists.
impl SettingItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>, current_value: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: if values.is_empty() { None } else { Some(values) },
            submenu: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Theme hooks for the settings list.
pub struct SettingsListTheme {
    pub label: LabelStyleFn,
    pub value: LabelStyleFn,
    pub description: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub cursor: String,
    pub hint: Box<dyn Fn(&str) -> String + Send + Sync>,
}

/// A plain theme (identity).
pub fn plain_settings_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(|s, _| s.to_string()),
        value: Box::new(|s, _| s.to_string()),
        description: Box::new(|s| s.to_string()),
        cursor: "→ ".to_string(),
        hint: Box::new(|s| s.to_string()),
    }
}

#[derive(Debug, Clone, Default)]
pub struct SettingsListOptions {
    pub enable_search: bool,
}

/// The settings list component.
pub struct SettingsList {
    items: Vec<SettingItem>,
    filtered_items: Vec<usize>, // indices into items
    theme: SettingsListTheme,
    selected_index: usize,
    max_visible: usize,
    search_input: Option<Input>,
    search_enabled: bool,
    submenu_component: Option<Box<dyn Component + Send + Sync>>,
    submenu_item_index: Option<usize>,
}

impl std::fmt::Debug for SettingsList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsList").field("selected", &self.selected_index).finish()
    }
}

impl SettingsList {
    pub fn new(items: Vec<SettingItem>, max_visible: usize, theme: SettingsListTheme, options: SettingsListOptions) -> Self {
        self_ready_init(items, max_visible, theme, options)
    }

    /// Update an item's currentValue.
    pub fn update_value(&mut self, id: &str, new_value: impl Into<String>) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value = new_value.into();
        }
    }

    pub fn select_item(&mut self, id: &str) {
        let items: Vec<&SettingItem> = if self.search_enabled {
            self.filtered_items.iter().map(|i| &self.items[*i]).collect()
        } else {
            self.items.iter().collect()
        };
        if let Some(index) = items.iter().position(|i| i.id == id) {
            self.selected_index = index;
        }
    }

    /// The selected item's id (used by the parent to persist changes).
    pub fn selected_id(&self) -> Option<String> {
        if self.search_enabled {
            self.filtered_items.get(self.selected_index).map(|i| self.items[*i].id.clone())
        } else {
            self.items.get(self.selected_index).map(|i| i.id.clone())
        }
    }

    pub fn visible_items(&self) -> Vec<&SettingItem> {
        if self.search_enabled {
            self.filtered_items.iter().map(|i| &self.items[*i]).collect()
        } else {
            self.items.iter().collect()
        }
    }

    /// Whether a submenu is currently open.
    pub fn is_submenu_open(&self) -> bool {
        self.submenu_component.is_some()
    }

    pub fn close_submenu(&mut self) {
        self.submenu_component = None;
        if let Some(idx) = self.submenu_item_index.take() {
            self.selected_index = idx;
        }
    }

    fn render_main_list(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        if self.search_enabled {
            if let Some(search) = &self.search_input {
                lines.extend(search.render(width));
                lines.push(String::new());
            }
        }

        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            self.add_hint_line(&mut lines, width);
            return lines;
        }

        let display_len = if self.search_enabled { self.filtered_items.len() } else { self.items.len() };
        if display_len == 0 {
            lines.push(truncate_to_width(&(self.theme.hint)("  No matching settings"), width, ""));
            self.add_hint_line(&mut lines, width);
            return lines;
        }

        let start_index = std::cmp::max(
            0,
            std::cmp::min(
                self.selected_index.saturating_sub(self.max_visible / 2),
                display_len.saturating_sub(self.max_visible),
            ),
        );
        let end_index = std::cmp::min(start_index + self.max_visible, display_len);
        let max_label_width = std::cmp::min(36, self.items.iter().map(|i| visible_width(&i.label)).max().unwrap_or(0));

        for i in start_index..end_index {
            let item = if self.search_enabled {
                &self.items[self.filtered_items[i]]
            } else {
                &self.items[i]
            };
            let is_selected = i == self.selected_index;
            let prefix = if is_selected { self.theme.cursor.clone() } else { "  ".to_string() };
            let prefix_width = visible_width(&prefix);

            let label_padded = format!("{}{}", item.label, " ".repeat(max_label_width.saturating_sub(visible_width(&item.label))));
            let label_text = (self.theme.label)(&label_padded, is_selected);
            let separator = "  ";
            let used_width = prefix_width + max_label_width + visible_width(separator);
            let value_max_width = width.saturating_sub(used_width + 2);
            let value_text = (self.theme.value)(&truncate_to_width(&item.current_value, value_max_width, ""), is_selected);
            lines.push(truncate_to_width(&format!("{prefix}{label_text}{separator}{value_text}"), width, ""));
        }

        if start_index > 0 || end_index < display_len {
            let scroll_text = format!("  ({}/{display_len})", self.selected_index + 1);
            lines.push((self.theme.hint)(&truncate_to_width(&scroll_text, width.saturating_sub(2), "")));
        }

        if let Some(selected_item) = if self.search_enabled {
            self.filtered_items.get(self.selected_index).map(|i| &self.items[*i])
        } else {
            self.items.get(self.selected_index)
        } {
            if let Some(description) = &selected_item.description {
                lines.push(String::new());
                for line in wrap_text_with_ansi(description, width.saturating_sub(4)) {
                    lines.push((self.theme.description)(&format!("  {line}")));
                }
            }
        }

        self.add_hint_line(&mut lines, width);
        lines
    }

    fn add_hint_line(&self, lines: &mut Vec<String>, width: usize) {
        lines.push(String::new());
        lines.push(truncate_to_width(
            &(self.theme.hint)(if self.search_enabled {
                "  Type to search · Enter/Space to change · Esc to cancel"
            } else {
                "  Enter/Space to change · Esc to cancel"
            }),
            width,
            "",
        ));
    }

    fn activate_item(&mut self) {
        let item = if self.search_enabled {
            self.filtered_items.get(self.selected_index).map(|i| &self.items[*i])
        } else {
            self.items.get(self.selected_index)
        };
        let Some(item) = item else { return };

        if let Some(submenu) = &item.submenu {
            let component = submenu(&item.current_value, false);
            if let Some(component) = component {
                self.submenu_item_index = Some(self.selected_index);
                self.submenu_component = Some(component);
            }
        } else if let Some(values) = &item.values {
            if !values.is_empty() {
                let current_index = values.iter().position(|v| *v == item.current_value).unwrap_or(0);
                let next_index = (current_index + 1) % values.len();
                let new_value = values[next_index].clone();
                let id = item.id.clone();
                self.update_value(&id, new_value.clone());
            }
        }
    }

    fn apply_filter(&mut self, query: &str) {
        let items = self.items.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        let filtered_labels: Vec<String> = self.items.iter().map(|i| i.label.clone()).collect();
        let _ = items;
        let matches = fuzzy_filter(filtered_labels, query, |label| label.clone());
        self.filtered_items = matches
            .iter()
            .filter_map(|label| self.items.iter().position(|i| &i.label == label))
            .collect();
        self.selected_index = 0;
    }
}

fn self_ready_init(mut items: Vec<SettingItem>, max_visible: usize, theme: SettingsListTheme, options: SettingsListOptions) -> SettingsList {
    let search_enabled = options.enable_search;
    let search_input = if search_enabled { Some(Input::new("")) } else { None };
    let filtered_items: Vec<usize> = (0..items.len()).collect();
    items.shrink_to_fit();
    SettingsList {
        items,
        filtered_items,
        theme,
        selected_index: 0,
        max_visible,
        search_input,
        search_enabled,
        submenu_component: None,
        submenu_item_index: None,
    }
}

impl Component for SettingsList {
    fn render(&self, width: usize) -> Vec<String> {
        if self.submenu_component.is_some() {
            let mut lines = Vec::new();
            if let Some(sub) = &self.submenu_component {
                lines.extend(sub.render(width));
            }
            return lines;
        }
        self.render_main_list(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        if self.submenu_component.is_some() {
            if let Some(sub) = &mut self.submenu_component {
                sub.handle_input(key);
            }
            return;
        }

        match key.base.as_str() {
            "up" => {
                let display_len = if self.search_enabled { self.filtered_items.len() } else { self.items.len() };
                if display_len == 0 {
                    return;
                }
                self.selected_index = if self.selected_index == 0 { display_len - 1 } else { self.selected_index - 1 };
            }
            "down" => {
                let display_len = if self.search_enabled { self.filtered_items.len() } else { self.items.len() };
                if display_len == 0 {
                    return;
                }
                self.selected_index = if self.selected_index == display_len - 1 { 0 } else { self.selected_index + 1 };
            }
            "enter" => {
                if key.ctrl {
                    return;
                }
                self.activate_item();
            }
            "escape" => {
                // Parent handles closing (returns via cancel).
            }
            _ => {
                if self.search_enabled {
                    let mut changed = false;
                    let mut new_value = String::new();
                    if let Some(search) = &mut self.search_input {
                        let before = search.value.clone();
                        search.handle_input(key);
                        if search.value != before {
                            changed = true;
                            new_value = search.value.clone();
                        }
                    }
                    if changed {
                        self.apply_filter(&new_value);
                    }
                }
            }
        }
    }

    fn invalidate(&mut self) {
        if let Some(sub) = &mut self.submenu_component {
            sub.invalidate();
        }
    }
}
