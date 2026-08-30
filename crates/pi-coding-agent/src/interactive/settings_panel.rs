//! Settings panel — a SettingsList-backed modal for interactive mode.

use pi_tui::components::input::Input;
use pi_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme, SettingsSubmenuChangeFn,
    SettingsSubmenuDoneFn,
};
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keybindings::get_keybindings;
use pi_tui::keys::TuiKey;
use pi_tui::tui::Component;
use pi_tui::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};
use std::sync::{Arc, Mutex};

use crate::interactive::tui_theme as t;

/// A settings entry builder.
pub struct SettingEntry {
    pub id: String,
    pub label: String,
    pub current_value: String,
    pub values: Option<Vec<String>>,
    pub description: Option<String>,
    pub disabled: bool,
    pub submenu_with_done: Option<SettingsSubmenuFactory>,
    pub submenu_with_callbacks: Option<SettingsSubmenuCallbacksFactory>,
    pub submenu_with_preview: Option<SettingsSubmenuPreviewFactory>,
}

impl std::fmt::Debug for SettingEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingEntry")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("current_value", &self.current_value)
            .field("values", &self.values)
            .field("description", &self.description)
            .field("disabled", &self.disabled)
            .field("has_submenu", &self.submenu_with_done.is_some())
            .field("has_live_submenu", &self.submenu_with_callbacks.is_some())
            .field("has_preview_submenu", &self.submenu_with_preview.is_some())
            .finish()
    }
}

impl Clone for SettingEntry {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            label: self.label.clone(),
            current_value: self.current_value.clone(),
            values: self.values.clone(),
            description: self.description.clone(),
            disabled: self.disabled,
            submenu_with_done: self.submenu_with_done.clone(),
            submenu_with_callbacks: self.submenu_with_callbacks.clone(),
            submenu_with_preview: self.submenu_with_preview.clone(),
        }
    }
}

/// Factory for a settings submenu. The factory receives the current value and
/// a completion callback that returns an optional selected value and optional
/// target entry id for chained navigation.
pub type SettingsSubmenuFactory = Arc<
    dyn Fn(&str, SettingsSubmenuDoneFn) -> Option<Box<dyn Component + Send + Sync>> + Send + Sync,
>;
pub type SettingsSubmenuCallbacksFactory = Arc<
    dyn Fn(
            &str,
            SettingsSubmenuDoneFn,
            SettingsSubmenuChangeFn,
        ) -> Option<Box<dyn Component + Send + Sync>>
        + Send
        + Sync,
>;
/// A callback emitted by a submenu while the user previews a value. Preview
/// values update the live UI but are not persisted until `done` commits.
pub type SettingsSubmenuPreviewFn = Box<dyn Fn(String) + Send + Sync>;
pub type SettingsSubmenuPreviewFactory = Arc<
    dyn Fn(
            &str,
            SettingsSubmenuDoneFn,
            SettingsSubmenuPreviewFn,
        ) -> Option<Box<dyn Component + Send + Sync>>
        + Send
        + Sync,
>;

/// A choice rendered by [`SettingChoiceSubmenu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChoice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl SettingChoice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl SettingEntry {
    pub fn cycle(id: &str, label: &str, current_value: String, values: Vec<String>) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            current_value,
            values: Some(values),
            description: None,
            disabled: false,
            submenu_with_done: None,
            submenu_with_callbacks: None,
            submenu_with_preview: None,
        }
    }
    pub fn info(id: &str, label: &str, current_value: String) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            current_value,
            values: None,
            description: None,
            disabled: false,
            submenu_with_done: None,
            submenu_with_callbacks: None,
            submenu_with_preview: None,
        }
    }
    pub fn describe(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Keep an entry visible while preventing selection and value changes.
    pub fn with_disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Attach a real submenu factory while retaining the entry's current
    /// value until the submenu reports a completed choice.
    pub fn with_submenu_done(
        mut self,
        submenu: impl Fn(&str, SettingsSubmenuDoneFn) -> Option<Box<dyn Component + Send + Sync>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.values = None;
        self.submenu_with_callbacks = None;
        self.submenu_with_preview = None;
        self.submenu_with_done = Some(Arc::new(submenu));
        self
    }

    /// Attach a submenu that can report live value changes while it remains
    /// open, matching Pi's nested warning/settings behavior.
    pub fn with_submenu_callbacks(
        mut self,
        submenu: impl Fn(
                &str,
                SettingsSubmenuDoneFn,
                SettingsSubmenuChangeFn,
            ) -> Option<Box<dyn Component + Send + Sync>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.values = None;
        self.submenu_with_done = None;
        self.submenu_with_preview = None;
        self.submenu_with_callbacks = Some(Arc::new(submenu));
        self
    }

    /// Attach a submenu with a transient preview callback. Preview values are
    /// delivered to the panel owner, while only completion is a setting
    /// change.
    pub fn with_submenu_preview(
        mut self,
        submenu: impl Fn(
                &str,
                SettingsSubmenuDoneFn,
                SettingsSubmenuPreviewFn,
            ) -> Option<Box<dyn Component + Send + Sync>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.values = None;
        self.submenu_with_done = None;
        self.submenu_with_callbacks = None;
        self.submenu_with_preview = Some(Arc::new(submenu));
        self
    }

    /// Build a reusable titled choice submenu for this setting.
    pub fn choice_submenu(
        id: &str,
        label: &str,
        current_value: String,
        description: impl Into<String>,
        choices: Vec<SettingChoice>,
    ) -> Self {
        let title = label.to_string();
        let description = description.into();
        let choices = Arc::new(choices);
        Self::info(id, label, current_value).with_submenu_done(move |current, done| {
            Some(Box::new(SettingChoiceSubmenu::new(
                title.clone(),
                description.clone(),
                choices.as_ref().clone(),
                current,
                done,
            )))
        })
    }
}

/// A reusable upstream-style single-step settings submenu.
///
/// The parent `SettingsList` owns the lifecycle and consumes the callback on
/// the next input dispatch. This component owns only the choice list, fuzzy
/// search, focus, and title/description rendering.
pub struct SettingChoiceSubmenu {
    title: String,
    description: String,
    choices: Vec<SettingChoice>,
    filtered_indices: Vec<usize>,
    selected_index: usize,
    search: Input,
    page_size: usize,
    done: Option<SettingsSubmenuDoneFn>,
}

impl std::fmt::Debug for SettingChoiceSubmenu {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingChoiceSubmenu")
            .field("title", &self.title)
            .field("choices", &self.choices)
            .field("selected_index", &self.selected_index)
            .finish()
    }
}

impl SettingChoiceSubmenu {
    pub fn new(
        title: impl Into<String>,
        description: impl Into<String>,
        choices: Vec<SettingChoice>,
        current_value: &str,
        done: SettingsSubmenuDoneFn,
    ) -> Self {
        let selected_index = choices
            .iter()
            .position(|choice| choice.value == current_value)
            .unwrap_or(0);
        let filtered_indices = (0..choices.len()).collect();
        Self {
            title: title.into(),
            description: description.into(),
            choices,
            filtered_indices,
            selected_index,
            search: Input::new("> "),
            page_size: 10,
            done: Some(done),
        }
    }

    fn apply_filter(&mut self) {
        let query = self.search.value.clone();
        let indices: Vec<usize> = (0..self.choices.len()).collect();
        self.filtered_indices = fuzzy_filter(indices, &query, |index| {
            let choice = &self.choices[*index];
            format!(
                "{} {} {}",
                choice.value,
                choice.label,
                choice.description.as_deref().unwrap_or_default()
            )
        });
        self.selected_index = 0;
    }

    fn finish(&mut self, selected_value: Option<String>) {
        let Some(done) = self.done.take() else {
            return;
        };
        done(selected_value, None);
    }

    fn move_selection(&mut self, direction: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        self.selected_index =
            (self.selected_index as isize + direction).rem_euclid(len as isize) as usize;
    }

    fn move_page(&mut self, direction: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        let step = self.page_size.max(1) as isize * direction.signum();
        self.selected_index =
            (self.selected_index as isize + step).rem_euclid(len as isize) as usize;
    }
}

impl Component for SettingChoiceSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![t::bold(t::fg("accent", &self.title))];
        if !self.description.is_empty() {
            lines.push(String::new());
            lines.extend(
                wrap_text_with_ansi(&self.description, width.saturating_sub(4))
                    .into_iter()
                    .map(|line| t::fg("muted", line)),
            );
        }

        lines.push(String::new());
        lines.extend(self.search.render(width));
        lines.push(String::new());

        if self.filtered_indices.is_empty() {
            lines.push(t::fg("dim", "  No matching settings"));
        } else {
            let max_label_width = self
                .filtered_indices
                .iter()
                .map(|index| visible_width(&self.choices[*index].label))
                .max()
                .unwrap_or(0)
                .min(32);
            let selected = self.selected_index.min(self.filtered_indices.len() - 1);
            let start = selected
                .saturating_sub(self.page_size / 2)
                .min(self.filtered_indices.len().saturating_sub(self.page_size));
            let end = (start + self.page_size).min(self.filtered_indices.len());
            for (offset, choice_index) in self.filtered_indices[start..end].iter().enumerate() {
                let choice = &self.choices[*choice_index];
                let selected = start + offset == self.selected_index;
                let prefix = if selected { "→ " } else { "  " };
                let label = format!(
                    "{}{}",
                    choice.label,
                    " ".repeat(max_label_width.saturating_sub(visible_width(&choice.label)))
                );
                let mut row = format!("{prefix}{label}");
                if let Some(description) = &choice.description {
                    let separator = "  ";
                    let available = width.saturating_sub(visible_width(&row) + separator.len() + 2);
                    if available > 10 {
                        row.push_str(separator);
                        row.push_str(&truncate_to_width(description, available, ""));
                    }
                }
                let row = truncate_to_width(&row, width, "");
                lines.push(if selected { t::fg("accent", row) } else { row });
            }
            if start > 0 || end < self.filtered_indices.len() {
                lines.push(t::fg(
                    "dim",
                    format!(
                        "  ({}/{})",
                        self.selected_index + 1,
                        self.filtered_indices.len()
                    ),
                ));
            }
        }

        lines.push(String::new());
        lines.push(t::fg(
            "dim",
            "  Type to filter · Enter to select · Esc to go back",
        ));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let bindings = get_keybindings();
        if bindings.matches(key, "tui.select.cancel") {
            self.finish(None);
            return;
        }
        if bindings.matches(key, "tui.select.up") {
            self.move_selection(-1);
            return;
        }
        if bindings.matches(key, "tui.select.down") {
            self.move_selection(1);
            return;
        }
        if bindings.matches(key, "tui.select.pageUp") {
            self.move_page(-1);
            return;
        }
        if bindings.matches(key, "tui.select.pageDown") {
            self.move_page(1);
            return;
        }
        if bindings.matches(key, "tui.select.confirm") {
            let value = self
                .filtered_indices
                .get(self.selected_index)
                .map(|index| self.choices[*index].value.clone());
            self.finish(value);
            return;
        }

        let before = self.search.value.clone();
        self.search.handle_input(key);
        if self.search.value != before {
            self.apply_filter();
        }
    }

    fn invalidate(&mut self) {
        self.search.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
    }
}

pub(crate) fn settings_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(|text, selected| {
            if selected {
                t::fg("accent", text)
            } else {
                text.to_string()
            }
        }),
        value: Box::new(|text, selected| {
            if selected {
                t::fg("accent", text)
            } else {
                t::fg("muted", text)
            }
        }),
        description: Box::new(|text| t::fg("dim", text)),
        cursor: t::fg("accent", "→ "),
        hint: Box::new(|text| t::fg("dim", text)),
    }
}

fn settings_border(width: usize) -> String {
    t::fg("border", "─".repeat(width.max(1)))
}

/// A modal settings panel bound to a SettingsManager.
pub struct SettingsPanel {
    list: SettingsList,
    /// Pending (id, value) changes to flush.
    changes: Arc<Mutex<Vec<(String, String)>>>,
    /// Pending transient preview values, kept separate from persisted
    /// changes so cancelling a submenu can restore the original UI state.
    previews: Arc<Mutex<Vec<(String, String)>>>,
    /// Parent-row values for callback-driven submenus. The nested callback
    /// payload is canonical for the runtime, but upstream keeps the parent
    /// row as a summary while the submenu remains open.
    submenu_display_values: Vec<(String, String)>,
}

impl SettingsPanel {
    pub fn new(entries: Vec<SettingEntry>) -> Self {
        let changes = Arc::new(Mutex::new(Vec::new()));
        let changes_for_items = changes.clone();
        let previews = Arc::new(Mutex::new(Vec::new()));
        let previews_for_items = previews.clone();
        let submenu_display_values = entries
            .iter()
            .filter(|entry| entry.submenu_with_callbacks.is_some())
            .map(|entry| (entry.id.clone(), entry.current_value.clone()))
            .collect();
        let items: Vec<SettingItem> = entries
            .iter()
            .map(|e| {
                let mut item = SettingItem::new(
                    &e.id,
                    &e.label,
                    &e.current_value,
                    e.values.clone().unwrap_or_default(),
                );
                if let Some(desc) = &e.description {
                    item.description = Some(desc.clone());
                }
                item.disabled = e.disabled;
                if let Some(submenu) = e.submenu_with_preview.clone() {
                    let id = e.id.clone();
                    let previews_for_entry = previews_for_items.clone();
                    item = item.with_submenu_done(move |current, done| {
                        let id = id.clone();
                        let previews_for_callback = previews_for_entry.clone();
                        let preview: SettingsSubmenuPreviewFn = Box::new(move |value| {
                            previews_for_callback
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push((id.clone(), value));
                        });
                        submenu(current, done, preview)
                    });
                } else if let Some(submenu) = e.submenu_with_callbacks.clone() {
                    let id = e.id.clone();
                    let changes_for_entry = changes_for_items.clone();
                    item = item.with_submenu_callbacks(move |current, done, _on_change| {
                        // SettingsList only drains its live queue on a later
                        // input dispatch, so callbacks emitted while the
                        // submenu is being constructed would otherwise be
                        // invisible to the panel owner for this turn.
                        let id = id.clone();
                        let changes_for_callback = changes_for_entry.clone();
                        let on_change: SettingsSubmenuChangeFn = Box::new(move |value| {
                            changes_for_callback
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push((id.clone(), value));
                        });
                        submenu(current, done, on_change)
                    });
                } else if let Some(submenu) = e.submenu_with_done.clone() {
                    item = item.with_submenu_done(move |current, done| submenu(current, done));
                }
                item
            })
            .collect();
        let changes_for_callback = changes.clone();
        let list = SettingsList::new_with_callbacks(
            items,
            10,
            settings_theme(),
            move |id, value| {
                changes_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push((id.to_string(), value.to_string()));
            },
            || {},
            SettingsListOptions {
                enable_search: true,
            },
        );
        Self {
            list,
            changes,
            previews,
            submenu_display_values,
        }
    }

    /// Drain pending value changes to flush back into the settings store.
    pub fn drain_changes(&mut self) -> Vec<(String, String)> {
        std::mem::take(
            &mut *self
                .changes
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    /// Drain transient submenu previews without treating them as persisted
    /// settings changes.
    pub fn drain_previews(&mut self) -> Vec<(String, String)> {
        std::mem::take(
            &mut *self
                .previews
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    /// Let the interactive owner distinguish an Escape that closes a nested
    /// settings submenu from an Escape that closes the whole modal.
    pub fn is_submenu_open(&self) -> bool {
        self.list.is_submenu_open()
    }

    /// Update a callback submenu's parent-row summary without emitting a
    /// second setting change. Nested selectors such as per-model thinking
    /// apply their canonical provider/model payload live, while the upstream
    /// parent row displays the derived summary until the submenu is closed.
    pub fn update_submenu_display_value(&mut self, id: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some((_, current)) = self
            .submenu_display_values
            .iter_mut()
            .find(|(entry_id, _)| entry_id == id)
        {
            *current = value.clone();
            self.list.update_value(id, value);
        }
    }

    /// Restore a regular setting row after a live callback rejects its
    /// requested value. This does not enqueue a persistence change; the owner
    /// supplies the authoritative value from its runtime state.
    pub fn update_value(&mut self, id: &str, value: impl Into<String>) {
        self.list.update_value(id, value);
    }
}

impl Component for SettingsPanel {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(settings_border(width));
        lines.extend(self.list.render(width));
        lines.push(settings_border(width));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        self.list.handle_input(key);
        for (id, value) in &self.submenu_display_values {
            self.list.update_value(id, value.clone());
        }
    }

    fn invalidate(&mut self) {
        self.list.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.list.set_focused(focused);
    }
}
