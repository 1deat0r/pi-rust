//! Selector components for interactive mode — model, thinking, theme,
//! settings, and session-info. Ports the selector surface from
//! `packages/coding-agent/src/modes/interactive/components/` (model-selector,
//! thinking-selector, theme-selector, settings-selector, session-selector).

use pi_tui::autocomplete::AutocompleteItem;
use pi_tui::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keys::TuiKey;
use pi_tui::tui::Component;

use crate::core::settings::SettingsManager;
use crate::interactive::tui_theme as t;

/// A simple select-theme built from the TUI theme colors.
fn select_theme() -> pi_tui::components::select_list::SelectListTheme {
    pi_tui::components::select_list::SelectListTheme {
        selected_prefix: Box::new(|s| s.to_string()),
        selected_text: Box::new(|s| t::bg("selectedBg", t::fg("selectedText", s))),
        description: Box::new(|s| t::fg("muted", s)),
        scroll_info: Box::new(|s| t::fg("muted", s)),
        no_match: Box::new(|s| t::fg("warning", s)),
    }
}

/// A running selector: renders a list and returns the picked value via the
/// `tick` result.
pub enum SelectorAction {
    None,
    Select(Option<usize>),
    Cancel,
    Cycle,
}

/// Modal list selector state.
pub struct ListSelector {
    list: SelectList,
}

impl ListSelector {
    pub fn new(items: Vec<SelectItem>, max_visible: usize) -> Self {
        Self {
            list: SelectList::new(
                items,
                max_visible,
                select_theme(),
                SelectListLayoutOptions::default(),
            ),
        }
    }

    pub fn new_slash_layout(items: Vec<SelectItem>, max_visible: usize) -> Self {
        Self {
            list: SelectList::new(
                items,
                max_visible,
                select_theme(),
                SelectListLayoutOptions {
                    min_primary_column_width: Some(12),
                    max_primary_column_width: Some(32),
                },
            ),
        }
    }

    pub fn selected_item(&self) -> Option<SelectItem> {
        self.list.get_selected_item().cloned()
    }

    pub fn selected_index(&self) -> usize {
        self.list.selected_index()
    }

    pub fn set_filter(&mut self, query: &str) {
        self.list.set_filter(query);
    }

    pub fn count(&self) -> usize {
        self.list.items().len()
    }

    /// Handle a key; returns a user-visible action.
    pub fn handle(&mut self, key: &TuiKey) -> SelectorAction {
        let base = key.base.as_str();
        if base == "up" || base == "down" {
            self.list.handle_input(key);
            SelectorAction::None
        } else if base == "enter" {
            SelectorAction::Select(Some(self.selected_index()))
        } else if base == "escape" || base == "esc" {
            SelectorAction::Cancel
        } else if base == "tab" {
            SelectorAction::Cycle
        } else {
            SelectorAction::None
        }
    }
}

impl Component for ListSelector {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

// ---------------------------------------------------------------------------
// Selector builders
// ---------------------------------------------------------------------------

/// Model list for the model selector (fuzzy-matched against provider/model).
pub fn model_selector_items(
    models: &pi_ai::models::Models,
    provider_filter: Option<&str>,
) -> Vec<SelectItem> {
    let providers = models.get_providers();
    let mut items = Vec::new();
    for provider in providers {
        let pid = provider.id.clone();
        if let Some(filter) = provider_filter {
            if pid != filter {
                continue;
            }
        }
        let mut model_ids: Vec<String> = models
            .get_models(Some(&pid))
            .into_iter()
            .map(|m| m.id.clone())
            .collect();
        model_ids.sort();
        model_ids.dedup();
        for mid in model_ids {
            if let Some(model) = models.get_model(&pid, &mid) {
                let label = format!("{pid}/{}", model.name);
                items.push(SelectItem {
                    value: format!("{pid}/{mid}"),
                    label,
                    description: Some(format!("{pid} — {} context", model.context_window)),
                });
            }
        }
    }
    items
}

/// Fuzzy-filter model items by a query (slash argument completion).
pub fn filter_model_items(items: &[SelectItem], query: &str) -> Vec<SelectItem> {
    fuzzy_filter(items.to_vec(), query, |item| item.value.clone())
}

/// Thinking level selector.
pub fn thinking_selector_items() -> Vec<SelectItem> {
    ["off", "low", "medium", "high", "xhigh"]
        .iter()
        .map(|level| SelectItem {
            value: level.to_string(),
            label: level.to_string(),
            description: match *level {
                "off" => Some("No reasoning effort".to_string()),
                "low" => Some("Minimal reasoning".to_string()),
                "medium" => Some("Balanced reasoning".to_string()),
                "high" => Some("High reasoning".to_string()),
                "xhigh" => Some("Maximum reasoning".to_string()),
                _ => None,
            },
        })
        .collect()
}

/// Theme selector: builtin dark + light, then custom themes from disk.
pub fn theme_selector_items() -> Vec<SelectItem> {
    let mut items = vec![
        SelectItem::new(
            "dark".to_string(),
            "dark".to_string(),
            Some("Dark theme".to_string()),
        ),
        SelectItem::new(
            "light".to_string(),
            "light".to_string(),
            Some("Light theme".to_string()),
        ),
    ];
    let themes_dir = crate::theme::custom_themes_dir();
    if let Ok(entries) = std::fs::read_dir(&themes_dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".json").map(|s| s.to_string())
            })
            .collect();
        names.sort();
        for name in names {
            items.push(SelectItem::new(
                name.clone(),
                name,
                Some("Custom theme".to_string()),
            ));
        }
    }
    items
}

/// Settings items for the settings selector.
pub fn settings_selector_items(
    settings: &SettingsManager,
) -> Vec<crate::interactive::settings_panel::SettingEntry> {
    use crate::interactive::settings_panel::SettingEntry;
    let theme = settings
        .get_theme_setting()
        .unwrap_or(crate::theme::DEFAULT_THEME)
        .to_string();
    let model_id = settings.get_default_model().unwrap_or("").to_string();
    let thinking = settings
        .get_default_thinking_level()
        .unwrap_or("off")
        .to_string();
    let images = if settings.get_show_images() {
        "on"
    } else {
        "off"
    };
    let cache_miss_notices = if settings.get_show_cache_miss_notices() {
        "true"
    } else {
        "false"
    };
    let install_telemetry = if settings.get_enable_install_telemetry() {
        "true"
    } else {
        "false"
    };
    vec![
        SettingEntry::cycle(
            "theme",
            "Theme",
            theme,
            vec!["dark".to_string(), "light".to_string()],
        ),
        SettingEntry::cycle(
            "thinking",
            "Default thinking level",
            thinking,
            vec![
                "off".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
            ],
        ),
        SettingEntry::cycle(
            "images",
            "Show images",
            images.to_string(),
            vec!["on".to_string(), "off".to_string()],
        ),
        SettingEntry::cycle(
            "cache-miss-notices",
            "Cache miss notices",
            cache_miss_notices.to_string(),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Show transcript notices for significant prompt-cache misses"),
        SettingEntry::cycle(
            "install-telemetry",
            "Install telemetry",
            install_telemetry.to_string(),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Send an anonymous version/update ping after changelog-detected updates"),
        SettingEntry::info("model", "Default model", model_id),
    ]
}

/// Build autocomplete items for @-attachments is handled by the provider;
/// this returns slash-command items for the editor.
pub fn slash_command_items() -> Vec<AutocompleteItem> {
    crate::interactive::slash::command_autocomplete_items()
}
