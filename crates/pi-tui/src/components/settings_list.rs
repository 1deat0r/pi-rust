//! SettingsList component — port of `packages/tui/src/components/settings-list.ts`.
//!
//! A settings screen: selectable items with a current value, cycling on
//! Enter/Space, optional submenu components, and optional search filtering.

use crate::components::input::Input;
use crate::fuzzy::fuzzy_filter;
use crate::keys::TuiKey;
use crate::tui::Component;
use crate::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

/// A submenu closure: (current value, expanded) -> optional component.
pub type SubmenuFn =
    Box<dyn Fn(&str, bool) -> Option<Box<dyn Component + Send + Sync>> + Send + Sync>;
/// A two-argument style function (text, selected).
pub type LabelStyleFn = Box<dyn Fn(&str, bool) -> String + Send + Sync>;
pub type SettingsChangeFn = Box<dyn Fn(&str, &str) + Send + Sync>;
pub type SettingsCancelFn = Box<dyn Fn() + Send + Sync>;
pub type SettingsSubmenuDoneFn = Box<dyn Fn(Option<String>, Option<String>) + Send + Sync>;
pub type SettingsSubmenuResult = (Option<String>, Option<String>);
pub type SettingsSubmenuState = std::sync::Arc<std::sync::Mutex<Option<SettingsSubmenuResult>>>;
pub type SubmenuWithDoneFn = Box<
    dyn Fn(&str, SettingsSubmenuDoneFn) -> Option<Box<dyn Component + Send + Sync>> + Send + Sync,
>;

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
    /// Callback-aware submenu variant. The second argument receives the
    /// selected value and optional target id when the submenu completes.
    pub submenu_with_done: Option<SubmenuWithDoneFn>,
    /// Disabled rows remain visible but cannot be selected or activated.
    pub disabled: bool,
}

impl std::fmt::Debug for SettingItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingItem")
            .field("id", &self.id)
            .field("label", &self.label)
            .finish()
    }
}

/// A minimal settings item for simple cycling lists.
impl SettingItem {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        current_value: impl Into<String>,
        values: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: if values.is_empty() {
                None
            } else {
                Some(values)
            },
            submenu: None,
            submenu_with_done: None,
            disabled: false,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn with_submenu_done(
        mut self,
        submenu: impl Fn(&str, SettingsSubmenuDoneFn) -> Option<Box<dyn Component + Send + Sync>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.submenu_with_done = Some(Box::new(submenu));
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
    navigate_after_close: Option<String>,
    submenu_done: Option<SettingsSubmenuState>,
    on_change: Option<SettingsChangeFn>,
    on_cancel: Option<SettingsCancelFn>,
}

impl std::fmt::Debug for SettingsList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsList")
            .field("selected", &self.selected_index)
            .finish()
    }
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        options: SettingsListOptions,
    ) -> Self {
        self_ready_init(items, max_visible, theme, options, None, None)
    }

    /// Construct a settings list with the upstream callback semantics.
    pub fn new_with_callbacks(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: impl Fn(&str, &str) + Send + Sync + 'static,
        on_cancel: impl Fn() + Send + Sync + 'static,
        options: SettingsListOptions,
    ) -> Self {
        self_ready_init(
            items,
            max_visible,
            theme,
            options,
            Some(Box::new(on_change)),
            Some(Box::new(on_cancel)),
        )
    }

    /// Attach callbacks after construction while retaining the legacy
    /// constructor used by existing callers.
    pub fn with_callbacks(
        mut self,
        on_change: impl Fn(&str, &str) + Send + Sync + 'static,
        on_cancel: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        self.on_change = Some(Box::new(on_change));
        self.on_cancel = Some(Box::new(on_cancel));
        self
    }

    /// Update an item's currentValue.
    pub fn update_value(&mut self, id: &str, new_value: impl Into<String>) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value = new_value.into();
        }
    }

    pub fn set_disabled(&mut self, id: &str, disabled: bool) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.disabled = disabled;
        }
    }

    pub fn select_item(&mut self, id: &str) {
        let items: Vec<&SettingItem> = if self.search_enabled {
            self.filtered_items
                .iter()
                .map(|i| &self.items[*i])
                .collect()
        } else {
            self.items.iter().collect()
        };
        if let Some(index) = items.iter().position(|i| i.id == id && !i.disabled) {
            self.selected_index = index;
        }
    }

    /// The selected item's id (used by the parent to persist changes).
    pub fn selected_id(&self) -> Option<String> {
        if self.search_enabled {
            self.filtered_items
                .get(self.selected_index)
                .map(|i| self.items[*i].id.clone())
        } else {
            self.items.get(self.selected_index).map(|i| i.id.clone())
        }
    }

    pub fn visible_items(&self) -> Vec<&SettingItem> {
        if self.search_enabled {
            self.filtered_items
                .iter()
                .map(|i| &self.items[*i])
                .collect()
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
        self.submenu_done = None;
        if let Some(id) = self.navigate_after_close.take() {
            self.select_item(&id);
            self.activate_item();
            return;
        }
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

        let display_len = if self.search_enabled {
            self.filtered_items.len()
        } else {
            self.items.len()
        };
        if display_len == 0 {
            lines.push(truncate_to_width(
                &(self.theme.hint)("  No matching settings"),
                width,
                "",
            ));
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
        let max_label_width = std::cmp::min(
            36,
            self.items
                .iter()
                .map(|i| visible_width(&i.label))
                .max()
                .unwrap_or(0),
        );

        for i in start_index..end_index {
            let item = if self.search_enabled {
                &self.items[self.filtered_items[i]]
            } else {
                &self.items[i]
            };
            let is_selected = i == self.selected_index;
            let prefix = if is_selected {
                self.theme.cursor.clone()
            } else {
                "  ".to_string()
            };
            let prefix_width = visible_width(&prefix);

            let label_padded = format!(
                "{}{}",
                item.label,
                " ".repeat(max_label_width.saturating_sub(visible_width(&item.label)))
            );
            let label_text = (self.theme.label)(&label_padded, is_selected);
            let separator = "  ";
            let used_width = prefix_width + max_label_width + visible_width(separator);
            let value_max_width = width.saturating_sub(used_width + 2);
            let value_text = (self.theme.value)(
                &truncate_to_width(&item.current_value, value_max_width, ""),
                is_selected,
            );
            lines.push(truncate_to_width(
                &format!("{prefix}{label_text}{separator}{value_text}"),
                width,
                "",
            ));
        }

        if start_index > 0 || end_index < display_len {
            let scroll_text = format!("  ({}/{display_len})", self.selected_index + 1);
            lines.push((self.theme.hint)(&truncate_to_width(
                &scroll_text,
                width.saturating_sub(2),
                "",
            )));
        }

        if let Some(selected_item) = if self.search_enabled {
            self.filtered_items
                .get(self.selected_index)
                .map(|i| &self.items[*i])
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
            self.filtered_items
                .get(self.selected_index)
                .map(|i| &self.items[*i])
        } else {
            self.items.get(self.selected_index)
        };
        let Some(item) = item else { return };
        if item.disabled {
            return;
        }

        if let Some(submenu_with_done) = &item.submenu_with_done {
            let result = std::sync::Arc::new(std::sync::Mutex::new(None));
            let result_for_callback = result.clone();
            let done: SettingsSubmenuDoneFn = Box::new(move |selected, navigate_to| {
                *result_for_callback.lock().unwrap() = Some((selected, navigate_to));
            });
            let component = submenu_with_done(&item.current_value, done);
            if let Some(component) = component {
                self.submenu_item_index = Some(self.selected_index);
                self.submenu_done = Some(result);
                self.submenu_component = Some(component);
            }
        } else if let Some(submenu) = &item.submenu {
            let component = submenu(&item.current_value, false);
            if let Some(component) = component {
                self.submenu_item_index = Some(self.selected_index);
                self.submenu_component = Some(component);
            }
        } else if let Some(values) = &item.values {
            if !values.is_empty() {
                let current_index = values
                    .iter()
                    .position(|v| *v == item.current_value)
                    .unwrap_or(0);
                let next_index = (current_index + 1) % values.len();
                let new_value = values[next_index].clone();
                let id = item.id.clone();
                self.update_value(&id, new_value.clone());
                if let Some(on_change) = &self.on_change {
                    on_change(&id, &new_value);
                }
            }
        }
    }

    fn apply_filter(&mut self, query: &str) {
        let indexed_items: Vec<usize> = (0..self.items.len()).collect();
        self.filtered_items = fuzzy_filter(indexed_items, query, |index| {
            self.items[*index].label.clone()
        });
        self.selected_index = self.first_enabled_index().unwrap_or(0);
    }

    fn consume_submenu_done(&mut self) {
        let Some(state) = &self.submenu_done else {
            return;
        };
        let result = state.lock().unwrap().take();
        let Some((selected_value, navigate_to)) = result else {
            return;
        };
        if let Some(value) = selected_value {
            let display_index = self.submenu_item_index.unwrap_or(self.selected_index);
            if let Some(item_index) = self.display_index(display_index) {
                let id = self.items[item_index].id.clone();
                self.update_value(&id, value.clone());
                if let Some(on_change) = &self.on_change {
                    on_change(&id, &value);
                }
            }
        }
        self.navigate_after_close = navigate_to;
        self.close_submenu();
    }

    fn display_index(&self, index: usize) -> Option<usize> {
        if self.search_enabled {
            self.filtered_items.get(index).copied()
        } else if index < self.items.len() {
            Some(index)
        } else {
            None
        }
    }

    fn first_enabled_index(&self) -> Option<usize> {
        let len = if self.search_enabled {
            self.filtered_items.len()
        } else {
            self.items.len()
        };
        (0..len).find(|index| {
            self.display_index(*index)
                .and_then(|item| self.items.get(item))
                .map(|item| !item.disabled)
                .unwrap_or(false)
        })
    }

    fn move_selection(&mut self, direction: isize) {
        let display_len = if self.search_enabled {
            self.filtered_items.len()
        } else {
            self.items.len()
        };
        if display_len == 0 || self.first_enabled_index().is_none() {
            return;
        }
        let mut index = self.selected_index.min(display_len - 1) as isize;
        for _ in 0..display_len {
            index = (index + direction).rem_euclid(display_len as isize);
            if self
                .display_index(index as usize)
                .and_then(|item| self.items.get(item))
                .map(|item| !item.disabled)
                .unwrap_or(false)
            {
                self.selected_index = index as usize;
                return;
            }
        }
    }
}

fn self_ready_init(
    mut items: Vec<SettingItem>,
    max_visible: usize,
    theme: SettingsListTheme,
    options: SettingsListOptions,
    on_change: Option<SettingsChangeFn>,
    on_cancel: Option<SettingsCancelFn>,
) -> SettingsList {
    let search_enabled = options.enable_search;
    let search_input = if search_enabled {
        Some(Input::new(""))
    } else {
        None
    };
    let filtered_items: Vec<usize> = (0..items.len()).collect();
    let selected_index = items.iter().position(|item| !item.disabled).unwrap_or(0);
    items.shrink_to_fit();
    SettingsList {
        items,
        filtered_items,
        theme,
        selected_index,
        max_visible: max_visible.max(1),
        search_input,
        search_enabled,
        submenu_component: None,
        submenu_item_index: None,
        navigate_after_close: None,
        submenu_done: None,
        on_change,
        on_cancel,
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
            self.consume_submenu_done();
            return;
        }

        match key.base.as_str() {
            "up" => {
                self.move_selection(-1);
            }
            "down" => {
                self.move_selection(1);
            }
            "enter" => {
                if key.ctrl {
                    return;
                }
                self.activate_item();
            }
            " " if !self.search_enabled
                || self
                    .search_input
                    .as_ref()
                    .map(|input| input.value.is_empty())
                    .unwrap_or(true) =>
            {
                if !key.ctrl {
                    self.activate_item();
                }
            }
            "escape" => {
                if let Some(on_cancel) = &self.on_cancel {
                    on_cancel();
                }
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
