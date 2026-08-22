//! Settings panel — a SettingsList-backed modal for interactive mode.

use pi_tui::components::settings_list::{SettingItem, SettingsList, SettingsListOptions, SettingsListTheme};
use pi_tui::tui::Component;
use pi_tui::keys::TuiKey;

use crate::interactive::tui_theme as t;

/// A settings entry builder.
#[derive(Debug, Clone)]
pub struct SettingEntry {
    pub id: String,
    pub label: String,
    pub current_value: String,
    pub values: Option<Vec<String>>,
    pub description: Option<String>,
}

impl SettingEntry {
    pub fn cycle(id: &str, label: &str, current_value: String, values: Vec<String>) -> Self {
        Self { id: id.to_string(), label: label.to_string(), current_value, values: Some(values), description: None }
    }
    pub fn info(id: &str, label: &str, current_value: String) -> Self {
        Self { id: id.to_string(), label: label.to_string(), current_value, values: None, description: None }
    }
    pub fn describe(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }
}

fn settings_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(|text, selected| {
            if selected {
                t::bold(t::fg("settingLabel", text))
            } else {
                t::fg("settingLabel", text)
            }
        }),
        value: Box::new(|text, selected| {
            if selected {
                t::fg("accent", text)
            } else {
                t::fg("settingValue", text)
            }
        }),
        description: Box::new(|text| t::fg("muted", text)),
        cursor: t::fg("accent", "→ "),
        hint: Box::new(|text| t::fg("muted", text)),
    }
}

/// A modal settings panel bound to a SettingsManager.
pub struct SettingsPanel {
    list: SettingsList,
    /// Pending (id, value) changes to flush.
    changes: Vec<(String, String)>,
}

impl SettingsPanel {
    pub fn new(entries: Vec<SettingEntry>) -> Self {
        let items: Vec<SettingItem> = entries
            .iter()
            .map(|e| {
                let mut item = SettingItem::new(&e.id, &e.label, &e.current_value, e.values.clone().unwrap_or_default());
                if let Some(desc) = &e.description {
                    item.description = Some(desc.clone());
                }
                item
            })
            .collect();
        let list = SettingsList::new(items, 12, settings_theme(), SettingsListOptions { enable_search: true });
        Self { list, changes: Vec::new() }
    }

    /// Drain pending value changes to flush back into the settings store.
    pub fn drain_changes(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.changes)
    }

    fn record_change(&mut self, id: &str, value: &str) {
        self.changes.push((id.to_string(), value.to_string()));
    }
}

impl Component for SettingsPanel {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        // Snapshot the selected visible item's value before handling.
        let before = self
            .list
            .visible_items()
            .get(self.selected_id_and_index().1)
            .map(|i| (i.id.clone(), i.current_value.clone()));
        self.list.handle_input(key);
        // On Enter/Space the SettingsList cycles the value; detect the change.
        if key.base == "enter" && !key.ctrl && !key.alt {
            if let Some((id, new_value)) = self
                .list
                .visible_items()
                .get(self.selected_id_and_index().1)
                .map(|i| (i.id.clone(), i.current_value.clone()))
            {
                if Some((id.clone(), new_value.clone())) != before {
                    self.record_change(&id, &new_value);
                }
            }
        }
    }
}

impl SettingsPanel {
    fn selected_id_and_index(&self) -> (Option<String>, usize) {
        let idx = match self.list.selected_id() {
            Some(id) if !self.list.visible_items().is_empty() => self
                .list
                .visible_items()
                .iter()
                .position(|i| i.id == id)
                .unwrap_or(0),
            _ => 0,
        };
        (self.list.selected_id(), idx)
    }
}
