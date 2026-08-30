//! Selector components for interactive mode — model, thinking, theme,
//! settings, and session-info. Ports the selector surface from
//! `packages/coding-agent/src/modes/interactive/components/` (model-selector,
//! thinking-selector, theme-selector, settings-selector, session-selector).

use pi_tui::autocomplete::AutocompleteItem;
use pi_tui::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsSubmenuChangeFn, SettingsSubmenuDoneFn,
};
use pi_tui::components::Input;
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keybindings::get_keybindings;
use pi_tui::keys::{match_key, TuiKey};
use pi_tui::tui::Component;
use pi_tui::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::core::project_trust::{
    get_project_trust_options, ProjectTrustOption, ProjectTrustStoreEntry, ProjectTrustUpdate,
};
use crate::core::settings::SettingsManager;
use crate::interactive::settings_panel::{settings_theme, SettingsSubmenuPreviewFn};
use crate::interactive::tui_theme as t;
use pi_ai::model::Model;

/// Thinking levels exposed by the pinned upstream selector, in display order.
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

const THINKING_LEVEL_DESCRIPTIONS: &[(&str, &str)] = &[
    ("off", "No reasoning"),
    ("minimal", "Very brief reasoning (~1k tokens)"),
    ("low", "Light reasoning (~2k tokens)"),
    ("medium", "Moderate reasoning (~8k tokens)"),
    ("high", "Deep reasoning (~16k tokens)"),
    ("xhigh", "Extra-high reasoning (~32k tokens)"),
    ("max", "Maximum reasoning"),
];

/// A simple select-theme built from the TUI theme colors.
fn select_theme_with_no_match(
    no_match: &'static str,
) -> pi_tui::components::select_list::SelectListTheme {
    pi_tui::components::select_list::SelectListTheme {
        selected_prefix: Box::new(|s| s.to_string()),
        selected_text: Box::new(|s| t::bg("selectedBg", t::fg("selectedText", s))),
        description: Box::new(|s| t::fg("muted", s)),
        scroll_info: Box::new(|s| t::fg("muted", s)),
        no_match: Box::new(move |_| t::fg("warning", no_match)),
    }
}

fn select_theme() -> pi_tui::components::select_list::SelectListTheme {
    select_theme_with_no_match("  No matching commands")
}

/// A running selector: renders a list and returns the picked value via the
/// `tick` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorAction {
    None,
    Select(Option<usize>),
    SelectAsDefault(Option<usize>),
    Cancel,
    Cycle,
}

/// Modal list selector state.
pub struct ListSelector {
    list: SelectList,
    items: Vec<SelectItem>,
    filtered_indices: Vec<usize>,
    search_input: Input,
    page_size: usize,
}

/// Actions emitted by the `/scoped-models` checklist. Enter toggles the
/// highlighted model and leaves the checklist open; Escape commits the
/// current set at the interactive-mode boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedModelsAction {
    None,
    Toggle { model: String, enabled: bool },
    Cancel,
}

/// Checklist used by `/scoped-models`. The model value remains the canonical
/// `provider/id` reference while the label carries the visible checkbox.
pub struct ScopedModelsSelector {
    list: SelectList,
    items: Vec<SelectItem>,
    enabled: Vec<String>,
    search_input: Input,
}

impl ScopedModelsSelector {
    pub fn new(items: Vec<SelectItem>, enabled: &[String]) -> Self {
        let mut all_items = items;
        let mut enabled_values = Vec::with_capacity(enabled.len());
        for value in enabled {
            if enabled_values.iter().any(|current| current == value) {
                continue;
            }
            enabled_values.push(value.clone());
            if !all_items.iter().any(|item| item.value == *value) {
                // Keep configured-but-unavailable models visible, matching the
                // upstream checklist's explicit unavailable row behavior.
                all_items.push(SelectItem::new(
                    value.clone(),
                    value.clone(),
                    Some("unavailable".to_owned()),
                ));
            }
        }
        let mut selector = Self {
            list: SelectList::new(
                Vec::new(),
                12,
                select_theme_with_no_match("  No matching models"),
                SelectListLayoutOptions {
                    min_primary_column_width: Some(16),
                    max_primary_column_width: Some(36),
                },
            ),
            items: all_items,
            enabled: enabled_values,
            search_input: Input::new("  Search: "),
        };
        selector.refresh(None, 0);
        selector
    }

    fn search_text(item: &SelectItem) -> String {
        model_search_text(item, true)
    }

    fn ordered_items(&self) -> Vec<SelectItem> {
        let mut ordered = Vec::with_capacity(self.items.len());
        for value in &self.enabled {
            if let Some(item) = self.items.iter().find(|item| item.value == *value) {
                ordered.push(item.clone());
            }
        }
        for item in &self.items {
            if !self.enabled.iter().any(|value| value == &item.value) {
                ordered.push(item.clone());
            }
        }
        ordered
    }

    fn display_items(&self, items: &[SelectItem]) -> Vec<SelectItem> {
        items
            .iter()
            .map(|item| {
                let marker = if self.enabled.iter().any(|value| value == &item.value) {
                    "[x]"
                } else {
                    "[ ]"
                };
                SelectItem::new(
                    item.value.clone(),
                    format!("{marker} {}", item.label),
                    item.description.clone(),
                )
            })
            .collect()
    }

    fn refresh(&mut self, selected_value: Option<&str>, fallback_index: usize) {
        let filtered = fuzzy_filter(
            self.ordered_items(),
            self.search_input.get_value(),
            Self::search_text,
        );
        let displayed = self.display_items(&filtered);
        self.list.set_items(displayed);
        if let Some(selected_value) = selected_value {
            if let Some(index) = self
                .list
                .items()
                .iter()
                .position(|item| item.value == selected_value)
            {
                self.list.set_selected_index(index);
                return;
            }
        }
        self.list.set_selected_index(fallback_index);
    }

    pub fn selected_models(&self) -> Vec<String> {
        self.enabled.clone()
    }

    pub fn search_query(&self) -> &str {
        self.search_input.get_value()
    }

    pub fn handle(&mut self, key: &TuiKey) -> ScopedModelsAction {
        let keybindings = get_keybindings();
        if keybindings.matches(key, "tui.select.up") || keybindings.matches(key, "tui.select.down")
        {
            self.list.handle_input(key);
            return ScopedModelsAction::None;
        }
        if keybindings.matches(key, "tui.select.confirm") {
            let Some(value) = self.list.get_selected_item().map(|item| item.value.clone()) else {
                return ScopedModelsAction::None;
            };
            let enabled =
                if let Some(index) = self.enabled.iter().position(|current| current == &value) {
                    self.enabled.remove(index);
                    false
                } else {
                    self.enabled.push(value.clone());
                    true
                };
            self.refresh(Some(&value), self.list.selected_index());
            return ScopedModelsAction::Toggle {
                model: value,
                enabled,
            };
        }
        // Upstream clears an active scoped-model search with Ctrl+C and only
        // cancels the checklist when the search field is already empty.
        if match_key(key, "ctrl+c") {
            if self.search_input.get_value().is_empty() {
                return ScopedModelsAction::Cancel;
            }
            self.search_input.clear();
            // Pi clears the query and lets the normal ordered list select its
            // first row again. Preserving the filtered row would make Ctrl+C
            // appear to move the selection unexpectedly after a search.
            self.refresh(None, 0);
            return ScopedModelsAction::None;
        }
        if keybindings.matches(key, "tui.select.cancel") {
            return ScopedModelsAction::Cancel;
        }
        let selected_value = self.list.get_selected_item().map(|item| item.value.clone());
        let selected_index = self.list.selected_index();
        let before = self.search_input.value.clone();
        self.search_input.handle_input(key);
        if self.search_input.value != before {
            // Search changes highlight the best match. Clearing the query
            // restores the selected item, like the upstream selector, while
            // a non-empty query starts at the best match.
            let cleared = self.search_input.value.trim().is_empty();
            self.refresh(
                cleared.then_some(selected_value.as_deref()).flatten(),
                if cleared { selected_index } else { 0 },
            );
        } else {
            self.refresh(selected_value.as_deref(), selected_index);
        }
        ScopedModelsAction::None
    }

    /// Return whether a canonical provider/model value is currently enabled.
    pub fn is_enabled(&self, value: &str) -> bool {
        self.enabled.iter().any(|current| current == value)
    }

    /// Number of enabled canonical model references, excluding duplicates.
    pub fn enabled_count(&self) -> usize {
        self.enabled.len()
    }

    /// Number of rows that are unavailable in the current model catalog.
    pub fn unavailable_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| {
                item.description
                    .as_deref()
                    .is_some_and(|description| description.eq_ignore_ascii_case("unavailable"))
            })
            .count()
    }

    /// Enable every row and retain the highlighted row when possible.
    pub fn enable_all(&mut self) {
        let selected_value = self.list.get_selected_item().map(|item| item.value.clone());
        self.enabled = self.items.iter().map(|item| item.value.clone()).collect();
        self.refresh(selected_value.as_deref(), self.list.selected_index());
    }

    /// Clear every enabled row and retain the highlighted row when possible.
    pub fn clear_all(&mut self) {
        let selected_value = self.list.get_selected_item().map(|item| item.value.clone());
        self.enabled.clear();
        self.refresh(selected_value.as_deref(), self.list.selected_index());
    }

    /// Move an enabled model within the persisted order. This is the pure
    /// state operation used by upstream's reorder affordance; key dispatch
    /// remains at the caller boundary until the modal enum grows a reorder
    /// action.
    pub fn move_enabled(&mut self, value: &str, delta: isize) -> bool {
        let Some(index) = self.enabled.iter().position(|current| current == value) else {
            return false;
        };
        let target = (index as isize + delta).clamp(0, self.enabled.len() as isize - 1) as usize;
        if target == index {
            return false;
        }
        let value = self.enabled.remove(index);
        self.enabled.insert(target, value);
        let selected_value = self.list.get_selected_item().map(|item| item.value.clone());
        self.refresh(selected_value.as_deref(), self.list.selected_index());
        true
    }
}

impl Component for ScopedModelsSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.search_input.render(width);
        lines.extend(self.list.render(width));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.set_focused(focused);
    }
}

impl ListSelector {
    pub fn new(items: Vec<SelectItem>, max_visible: usize) -> Self {
        Self::new_with_layout(items, max_visible, SelectListLayoutOptions::default())
    }

    fn new_with_layout(
        items: Vec<SelectItem>,
        max_visible: usize,
        layout: SelectListLayoutOptions,
    ) -> Self {
        let filtered_indices = (0..items.len()).collect();
        Self {
            list: SelectList::new(items.clone(), max_visible, select_theme(), layout),
            items,
            filtered_indices,
            search_input: Input::new("  Search: "),
            page_size: max_visible.max(1),
        }
    }

    pub fn new_slash_layout(items: Vec<SelectItem>, max_visible: usize) -> Self {
        Self::new_with_layout(
            items,
            max_visible,
            SelectListLayoutOptions {
                min_primary_column_width: Some(12),
                max_primary_column_width: Some(32),
            },
        )
    }

    pub fn selected_item(&self) -> Option<SelectItem> {
        self.list.get_selected_item().cloned()
    }

    /// Return the selected value as a stable canonical string for callers
    /// that persist/report a choice after filtering or paging.
    pub fn selected_value(&self) -> Option<String> {
        self.selected_item().map(|item| item.value)
    }

    pub fn selected_index(&self) -> usize {
        self.list.selected_index()
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.list.set_selected_index(index);
    }

    pub fn set_filter(&mut self, query: &str) {
        self.search_input.set_value(query);
        self.apply_filter();
    }

    pub fn search_query(&self) -> &str {
        self.search_input.get_value()
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    /// Number of rows matching the current query.
    pub fn filtered_count(&self) -> usize {
        self.filtered_indices.len()
    }

    /// Return the original item index highlighted by the filtered list.
    pub fn selected_original_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected_index()).copied()
    }

    /// Handle a key; returns a user-visible action.
    pub fn handle(&mut self, key: &TuiKey) -> SelectorAction {
        let keybindings = get_keybindings();
        if keybindings.matches(key, "tui.select.up") || keybindings.matches(key, "tui.select.down")
        {
            self.list.handle_input(key);
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.pageUp") {
            self.move_page(-1);
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.pageDown") {
            self.move_page(1);
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.confirm") {
            return self
                .filtered_indices
                .get(self.selected_index())
                .copied()
                .map(|index| SelectorAction::Select(Some(index)))
                .unwrap_or(SelectorAction::None);
        }
        if keybindings.matches(key, "tui.select.cancel") {
            return SelectorAction::Cancel;
        }
        if keybindings.matches(key, "tui.input.tab") {
            return SelectorAction::Cycle;
        }
        let before = self.search_input.value.clone();
        // Input owns all configured editing behavior (grapheme-safe cursor
        // movement, word deletion, kill/yank, undo, and custom bindings).
        // Do not special-case Ctrl+H here: on Unix terminals it is parsed as
        // backspace already, while a user binding must remain authoritative.
        self.search_input.handle_input(key);
        if self.search_input.value != before {
            self.apply_filter();
        }
        SelectorAction::None
    }

    fn move_page(&mut self, direction: isize) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let last = self.filtered_indices.len() - 1;
        let step = self.page_size as isize * direction;
        self.list.set_selected_index(
            (self.list.selected_index() as isize + step).clamp(0, last as isize) as usize,
        );
    }

    fn apply_filter(&mut self) {
        let query = self.search_input.get_value();
        let previous_selected_index = self.list.selected_index();
        let indexed = self.items.iter().cloned().enumerate().collect::<Vec<_>>();
        let filtered = fuzzy_filter(indexed, query, |entry| model_search_text(&entry.1, false));
        self.filtered_indices = filtered.iter().map(|(index, _)| *index).collect();
        self.list.set_items(
            filtered
                .into_iter()
                .map(|(_, item)| item)
                .collect::<Vec<_>>(),
        );
        if query.trim().is_empty() {
            // The upstream selectors preserve the current visual index when
            // the search is cleared; they do not jump back to the previously
            // highlighted original item in the filtered result.
            self.list.set_selected_index(previous_selected_index);
        }
    }
}

impl Component for ListSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.search_input.render(width);
        lines.extend(self.list.render(width));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.set_focused(focused);
    }
}

fn selector_border(width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        t::fg("border", "─".repeat(width))
    }
}

fn selector_line(text: &str, width: usize) -> String {
    truncate_to_width(text, width, "…")
}

/// The bordered thinking-level selector used by the thinking command.
///
/// The generic list selector remains useful for the older theme/fork/resume
/// modals, but upstream gives thinking its own header, hint, input prompt,
/// footer, and full-width borders.
pub struct ThinkingSelector {
    list: SelectList,
    items: Vec<SelectItem>,
    filtered_indices: Vec<usize>,
    page_size: usize,
    search_input: Input,
}

impl ThinkingSelector {
    pub fn new(items: Vec<SelectItem>, current_level: &str, default_level: Option<&str>) -> Self {
        let items = items
            .into_iter()
            .map(|mut item| {
                if Some(item.value.as_str()) == default_level {
                    item.description = Some(match item.description.take() {
                        Some(description) => format!("{description} · default"),
                        None => "default".to_string(),
                    });
                }
                item
            })
            .collect::<Vec<_>>();
        let filtered_indices = (0..items.len()).collect::<Vec<_>>();
        let page_size = items.len().max(1);
        let mut selector = Self {
            list: SelectList::new(
                items.clone(),
                items.len().max(1),
                select_theme_with_no_match("  No matching thinking levels"),
                SelectListLayoutOptions {
                    min_primary_column_width: Some(12),
                    max_primary_column_width: Some(32),
                },
            ),
            items,
            filtered_indices,
            search_input: Input::new("> "),
            page_size,
        };
        if let Some(index) = selector
            .items
            .iter()
            .position(|item| item.value == current_level)
        {
            selector.list.set_selected_index(index);
        }
        selector
    }

    pub fn selected_item(&self) -> Option<SelectItem> {
        self.list.get_selected_item().cloned()
    }

    /// Return the selected thinking level independently of the action's
    /// filtered-list index.
    pub fn selected_thinking_level(&self) -> Option<String> {
        self.selected_item().map(|item| item.value)
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    fn apply_filter(&mut self, preserve_value: Option<&str>) {
        let filtered = fuzzy_filter(
            self.items.iter().cloned().enumerate().collect::<Vec<_>>(),
            self.search_input.get_value(),
            |entry| {
                format!(
                    "{} {} {}",
                    entry.1.value,
                    entry.1.label,
                    entry.1.description.as_deref().unwrap_or_default()
                )
            },
        );
        self.filtered_indices = filtered.iter().map(|(index, _)| *index).collect();
        self.list.set_items(
            filtered
                .into_iter()
                .map(|(_, item)| item)
                .collect::<Vec<_>>(),
        );
        if let Some(value) = preserve_value {
            if let Some(index) = self
                .list
                .items()
                .iter()
                .position(|item| item.value == value)
            {
                self.list.set_selected_index(index);
                return;
            }
        }
        self.list.set_selected_index(0);
    }

    pub fn handle(&mut self, key: &TuiKey) -> SelectorAction {
        let keybindings = get_keybindings();
        if keybindings.matches(key, "tui.select.up") || keybindings.matches(key, "tui.select.down")
        {
            self.list.handle_input(key);
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.pageUp") {
            self.move_page(-1);
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.pageDown") {
            self.move_page(1);
            return SelectorAction::None;
        }
        if match_key(key, "ctrl+s") {
            return self
                .filtered_indices
                .get(self.list.selected_index())
                .copied()
                .map(|index| SelectorAction::SelectAsDefault(Some(index)))
                .unwrap_or(SelectorAction::None);
        }
        if keybindings.matches(key, "tui.select.confirm") {
            return self
                .filtered_indices
                .get(self.list.selected_index())
                .copied()
                .map(|index| SelectorAction::Select(Some(index)))
                .unwrap_or(SelectorAction::None);
        }
        if keybindings.matches(key, "tui.select.cancel") {
            return SelectorAction::Cancel;
        }
        let selected_value = self.selected_item().map(|item| item.value);
        let before = self.search_input.value.clone();
        self.search_input.handle_input(key);
        if self.search_input.value != before {
            self.apply_filter(selected_value.as_deref());
        }
        SelectorAction::None
    }

    fn move_page(&mut self, direction: isize) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let last = self.filtered_indices.len() - 1;
        let step = self.page_size as isize * direction;
        self.list.set_selected_index(
            (self.list.selected_index() as isize + step).clamp(0, last as isize) as usize,
        );
    }
}

impl Component for ThinkingSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![
            selector_border(width),
            String::new(),
            selector_line("Thinking Level", width),
            String::new(),
            selector_line("Shift+Tab cycles thinking levels in-session", width),
            String::new(),
        ];
        lines.extend(self.search_input.render(width));
        lines.push(String::new());
        lines.extend(self.list.render(width));
        lines.push(String::new());
        lines.push(t::dim(selector_line(
            "  Enter to select · Ctrl+S to set as default · Esc to cancel",
            width,
        )));
        lines.push(selector_border(width));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.set_focused(focused);
    }
}

fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn models_for_references(models: &[Model], references: &[String]) -> Vec<Model> {
    references
        .iter()
        .filter_map(|reference| {
            models
                .iter()
                .find(|model| model_key(model) == *reference)
                .cloned()
        })
        .collect()
}

fn model_search_text_model(model: &Model) -> String {
    format!(
        "{} {} {}/{} {}",
        model.id, model.provider, model.provider, model.id, model.name
    )
}

fn is_default_search(query: &str) -> bool {
    let normalized = query.trim().to_ascii_lowercase();
    !normalized.is_empty() && "default".starts_with(&normalized)
}

/// The configured/authenticated model selector used by the model command.
///
/// Unlike the generic list selector, this owns the model metadata footer and
/// catalog-refresh status line that Pi renders below the visible rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelScope {
    All,
    Scoped,
}

pub struct ModelSelector {
    all_models: Vec<Model>,
    scoped_models: Vec<Model>,
    scoped_model_refs: Vec<String>,
    filtered_models: Vec<Model>,
    selected_index: usize,
    page_size: usize,
    search_input: Input,
    current_model: Option<String>,
    default_model: Option<String>,
    refresh_status_message: String,
    refresh_status_success: bool,
    error_message: Option<String>,
    closed: bool,
    scope: ModelScope,
}

impl ModelSelector {
    pub fn new(
        models: Vec<Model>,
        current_model: Option<String>,
        default_model: Option<String>,
    ) -> Self {
        Self::new_with_scoped_models(models, &[], current_model, default_model)
    }

    /// Construct a model selector with the session's optional model scope.
    ///
    /// The upstream selector opens on the scoped list when one exists and
    /// keeps the configured order for that list. The Rust runtime stores the
    /// scope as canonical `provider/id` references, so resolve those against
    /// the available snapshot here while retaining the references for catalog
    /// refreshes.
    pub fn new_with_scoped_models(
        mut models: Vec<Model>,
        scoped_model_refs: &[String],
        current_model: Option<String>,
        default_model: Option<String>,
    ) -> Self {
        models.sort_by(|left, right| {
            let left_key = model_key(left);
            let right_key = model_key(right);
            let left_current = Some(left_key.as_str()) == current_model.as_deref();
            let right_current = Some(right_key.as_str()) == current_model.as_deref();
            right_current
                .cmp(&left_current)
                .then_with(|| {
                    let left_default = Some(left_key.as_str()) == default_model.as_deref();
                    let right_default = Some(right_key.as_str()) == default_model.as_deref();
                    right_default.cmp(&left_default)
                })
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.id.cmp(&right.id))
        });
        models.dedup_by(|right, left| left.provider == right.provider && left.id == right.id);
        let scoped_model_refs = scoped_model_refs
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|reference| !reference.is_empty())
            .map(str::to_owned)
            .fold(Vec::new(), |mut references, reference| {
                if !references.iter().any(|current| current == &reference) {
                    references.push(reference);
                }
                references
            });
        let scoped_models = models_for_references(&models, &scoped_model_refs);
        let scope = if scoped_models.is_empty() {
            ModelScope::All
        } else {
            ModelScope::Scoped
        };
        let mut selector = Self {
            all_models: models,
            scoped_models,
            scoped_model_refs,
            filtered_models: Vec::new(),
            selected_index: 0,
            page_size: 10,
            search_input: Input::new("> "),
            current_model,
            default_model,
            refresh_status_message: "Refreshing model catalogs…".to_string(),
            refresh_status_success: false,
            error_message: None,
            closed: false,
            scope,
        };
        selector.refilter(None);
        selector.selected_index = selector
            .filtered_models
            .iter()
            .position(|model| selector.is_current(model))
            .unwrap_or(0);
        selector
    }

    fn active_models(&self) -> &[Model] {
        match self.scope {
            ModelScope::All => &self.all_models,
            ModelScope::Scoped => &self.scoped_models,
        }
    }

    fn rebuild_scoped_models(&mut self, previous: &[Model]) {
        self.scoped_models = self
            .scoped_model_refs
            .iter()
            .filter_map(|reference| {
                self.all_models
                    .iter()
                    .find(|model| model_key(model) == *reference)
                    .cloned()
                    .or_else(|| {
                        previous
                            .iter()
                            .find(|model| model_key(model) == *reference)
                            .cloned()
                    })
            })
            .collect();
        if self.scoped_models.is_empty() {
            self.scope = ModelScope::All;
        } else if self.scope == ModelScope::All && !self.scoped_model_refs.is_empty() {
            // A refresh can make a previously unresolved scope available.
            // Restore the scoped view once it has concrete rows to show.
            self.scope = ModelScope::Scoped;
        }
    }

    fn scope_has_rows(&self) -> bool {
        !self.scoped_models.is_empty()
    }

    fn is_current(&self, model: &Model) -> bool {
        self.current_model
            .as_deref()
            .is_some_and(|current| current == model_key(model))
    }

    fn is_default(&self, model: &Model) -> bool {
        self.default_model
            .as_deref()
            .is_some_and(|default| default == model_key(model))
    }

    fn sort_models(&mut self) {
        let current_model = self.current_model.clone();
        let default_model = self.default_model.clone();
        self.all_models.sort_by(|left, right| {
            let left_key = model_key(left);
            let right_key = model_key(right);
            let left_current = Some(left_key.as_str()) == current_model.as_deref();
            let right_current = Some(right_key.as_str()) == current_model.as_deref();
            right_current
                .cmp(&left_current)
                .then_with(|| {
                    let left_default = Some(left_key.as_str()) == default_model.as_deref();
                    let right_default = Some(right_key.as_str()) == default_model.as_deref();
                    right_default.cmp(&left_default)
                })
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.id.cmp(&right.id))
        });
    }

    fn refilter(&mut self, preserve_value: Option<&str>) {
        let query = self.search_input.get_value();
        let active_models = self.active_models().to_vec();
        self.filtered_models = if query.trim().is_empty() {
            active_models
        } else {
            let filtered = fuzzy_filter(active_models.clone(), query, |model| {
                let mut text = model_search_text_model(model);
                if self.is_default(model) {
                    text.push_str(" default");
                }
                text
            });
            if is_default_search(query) {
                let defaults = self
                    .active_models()
                    .iter()
                    .filter(|model| self.is_default(model))
                    .cloned()
                    .collect::<Vec<_>>();
                let default_keys = defaults.iter().map(model_key).collect::<Vec<_>>();
                defaults
                    .into_iter()
                    .chain(
                        filtered
                            .into_iter()
                            .filter(|model| !default_keys.contains(&model_key(model))),
                    )
                    .collect()
            } else {
                filtered
            }
        };
        self.selected_index = if query.trim().is_empty() {
            preserve_value
                .and_then(|value| {
                    self.filtered_models
                        .iter()
                        .position(|model| model_key(model) == value)
                })
                .unwrap_or(0)
        } else {
            0
        }
        .min(self.filtered_models.len().saturating_sub(1));
    }

    pub fn selected_model(&self) -> Option<Model> {
        self.filtered_models.get(self.selected_index).cloned()
    }

    /// Return the selected model as `provider/id`, suitable for persistence.
    pub fn selected_model_reference(&self) -> Option<String> {
        self.selected_model().map(|model| model_key(&model))
    }

    pub fn count(&self) -> usize {
        self.filtered_models.len()
    }

    pub fn handle(&mut self, key: &TuiKey) -> SelectorAction {
        let keybindings = get_keybindings();
        if keybindings.matches(key, "tui.input.tab") {
            if self.scope_has_rows() {
                self.scope = match self.scope {
                    ModelScope::All => ModelScope::Scoped,
                    ModelScope::Scoped => ModelScope::All,
                };
                let current_model = self.current_model.clone();
                self.refilter(current_model.as_deref());
            }
            return SelectorAction::Cycle;
        }
        if keybindings.matches(key, "tui.select.up") {
            if !self.filtered_models.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_models.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.down") {
            if !self.filtered_models.is_empty() {
                self.selected_index = if self.selected_index + 1 >= self.filtered_models.len() {
                    0
                } else {
                    self.selected_index + 1
                };
            }
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.pageUp") {
            self.move_page(-1);
            return SelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.pageDown") {
            self.move_page(1);
            return SelectorAction::None;
        }
        if match_key(key, "ctrl+s") {
            return self
                .selected_model()
                .map(|_| SelectorAction::SelectAsDefault(Some(self.selected_index)))
                .unwrap_or(SelectorAction::None);
        }
        if keybindings.matches(key, "tui.select.confirm") {
            return self
                .selected_model()
                .map(|_| SelectorAction::Select(Some(self.selected_index)))
                .unwrap_or(SelectorAction::None);
        }
        if keybindings.matches(key, "tui.select.cancel") {
            self.dispose();
            return SelectorAction::Cancel;
        }
        let previous = self.selected_model().map(|model| model_key(&model));
        let before = self.search_input.value.clone();
        self.search_input.handle_input(key);
        if self.search_input.value != before {
            self.refilter(if self.search_input.get_value().trim().is_empty() {
                previous.as_deref()
            } else {
                None
            });
        }
        SelectorAction::None
    }

    fn move_page(&mut self, direction: isize) {
        if self.filtered_models.is_empty() {
            return;
        }
        let last = self.filtered_models.len() - 1;
        let step = self.page_size as isize * direction;
        self.selected_index =
            (self.selected_index as isize + step).clamp(0, last as isize) as usize;
    }

    /// Publish a refreshed authenticated snapshot without allowing a closed
    /// modal's background task to mutate a stale component.
    pub fn apply_refresh(
        &mut self,
        mut models: Vec<Model>,
        result: &pi_ai::models::ModelsRefreshResult,
    ) {
        if self.closed {
            return;
        }
        let selected = self.selected_model().map(|model| model_key(&model));
        let previous_scoped_models = self.scoped_models.clone();
        self.all_models = std::mem::take(&mut models);
        self.all_models.sort_by(|left, right| {
            model_key(left)
                .cmp(&model_key(right))
                .then_with(|| left.name.cmp(&right.name))
        });
        self.all_models
            .dedup_by(|right, left| left.provider == right.provider && left.id == right.id);
        self.sort_models();
        self.rebuild_scoped_models(&previous_scoped_models);
        self.refilter(selected.as_deref());
        self.refresh_status_message.clear();
        self.error_message = if result.aborted {
            Some("Model refresh timed out; showing cached models.".to_string())
        } else if result.errors.len() == 1 {
            result
                .errors
                .keys()
                .next()
                .map(|provider| format!("Could not refresh {provider}; showing cached models."))
        } else if result.errors.len() > 1 {
            let providers = result.errors.keys().cloned().collect::<Vec<_>>().join(", ");
            Some(format!(
                "Could not refresh {} model catalogs ({providers}); showing cached models.",
                result.errors.len()
            ))
        } else {
            self.refresh_status_success = true;
            self.refresh_status_message = "Model catalogs refreshed.".to_string();
            None
        };
    }

    pub fn dispose(&mut self) {
        self.closed = true;
    }
}

impl Component for ModelSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![selector_border(width), String::new()];
        if self.scope_has_rows() {
            let all = if self.scope == ModelScope::All {
                t::fg("accent", "all")
            } else {
                t::fg("muted", "all")
            };
            let scoped = if self.scope == ModelScope::Scoped {
                t::fg("accent", "scoped")
            } else {
                t::fg("muted", "scoped")
            };
            lines.push(selector_line(&format!("Scope: {all} | {scoped}"), width));
            lines.push(t::dim(selector_line("Tab scope (all/scoped)", width)));
        } else {
            lines.push(t::fg(
                "warning",
                selector_line(
                    "Only showing models from configured providers. Use /login to add providers.",
                    width,
                ),
            ));
        }
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        let max_visible = 10;
        if self.filtered_models.is_empty() {
            if let Some(error) = &self.error_message {
                lines.push(t::fg("error", selector_line(error, width)));
            } else {
                lines.push(t::fg("muted", selector_line("  No matching models", width)));
            }
        } else {
            let start = self
                .selected_index
                .saturating_sub(max_visible / 2)
                .min(self.filtered_models.len().saturating_sub(max_visible));
            let end = (start + max_visible).min(self.filtered_models.len());
            for (index, model) in self.filtered_models[start..end].iter().enumerate() {
                let index = start + index;
                let prefix = if index == self.selected_index {
                    "→ "
                } else {
                    "  "
                };
                let default_badge = if self.is_default(model) {
                    " · default"
                } else {
                    ""
                };
                let current_badge = if self.is_current(model) { " ✓" } else { "" };
                let line = format!(
                    "{prefix}{} [{}]{default_badge}{current_badge}",
                    model.id, model.provider
                );
                let color = if index == self.selected_index {
                    "accent"
                } else {
                    "muted"
                };
                lines.push(t::fg(color, selector_line(&line, width)));
            }
            if start > 0 || end < self.filtered_models.len() {
                lines.push(t::fg(
                    "muted",
                    selector_line(
                        &format!(
                            "  ({}/{})",
                            self.selected_index + 1,
                            self.filtered_models.len()
                        ),
                        width,
                    ),
                ));
            }
            if let Some(model) = self.selected_model() {
                lines.push(String::new());
                lines.push(t::fg(
                    "muted",
                    selector_line(&format!("  Model Name: {}", model.name), width),
                ));
            }
            if let Some(error) = &self.error_message {
                lines.push(String::new());
                lines.push(t::fg("error", selector_line(error, width)));
            }
        }
        if !self.refresh_status_message.is_empty() {
            lines.push(String::new());
            let color = if self.refresh_status_success {
                "success"
            } else {
                "muted"
            };
            lines.push(t::fg(
                color,
                selector_line(&format!("  {}", self.refresh_status_message), width),
            ));
        }
        lines.push(String::new());
        lines.push(t::dim(selector_line(
            "  Enter to select · Ctrl+S to set as default · Esc to cancel",
            width,
        )));
        lines.push(selector_border(width));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.set_focused(focused);
    }
}

/// The durable/session-only trust choice returned by the trust selector.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustSelection {
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
}

/// Actions emitted by TrustSelector. The caller owns persistence and
/// extension hooks; this component only owns deterministic keyboard state.
#[derive(Debug, Clone, PartialEq)]
pub enum TrustSelectorAction {
    None,
    Select(TrustSelection),
    Cancel,
}

/// Project-trust selector matching the pinned trust-selector.ts surface.
/// It intentionally has no callback or filesystem side effect so it can be
/// embedded in either startup UI or an interactive modal safely.
pub struct TrustSelector {
    cwd: String,
    options: Vec<ProjectTrustOption>,
    saved_decision: Option<ProjectTrustStoreEntry>,
    project_trusted: bool,
    selected: usize,
}

impl TrustSelector {
    pub fn new(
        cwd: impl Into<String>,
        saved_decision: Option<ProjectTrustStoreEntry>,
        project_trusted: bool,
    ) -> Self {
        let cwd = cwd.into();
        let options = get_project_trust_options(&cwd, false);
        let selected = options
            .iter()
            .position(|option| {
                option.saved_path.as_deref()
                    == saved_decision.as_ref().map(|entry| entry.path.as_str())
                    && option.trusted == saved_decision.as_ref().is_some_and(|entry| entry.decision)
            })
            .unwrap_or(0);
        Self {
            cwd,
            options,
            saved_decision,
            project_trusted,
            selected,
        }
    }

    pub fn options(&self) -> &[ProjectTrustOption] {
        &self.options
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_option(&self) -> Option<&ProjectTrustOption> {
        self.options.get(self.selected)
    }

    pub fn handle(&mut self, key: &TuiKey) -> TrustSelectorAction {
        let keybindings = get_keybindings();
        let plain_key = !key.ctrl && !key.shift && !key.alt;
        if keybindings.matches(key, "tui.select.up") || (plain_key && key.base == "k") {
            self.selected = self.selected.saturating_sub(1);
            return TrustSelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.down") || (plain_key && key.base == "j") {
            if !self.options.is_empty() {
                self.selected = (self.selected + 1).min(self.options.len() - 1);
            }
            return TrustSelectorAction::None;
        }
        if keybindings.matches(key, "tui.select.confirm") {
            return self
                .selected_option()
                .map(|option| {
                    TrustSelectorAction::Select(TrustSelection {
                        trusted: option.trusted,
                        updates: option.updates.clone(),
                    })
                })
                .unwrap_or(TrustSelectorAction::None);
        }
        if keybindings.matches(key, "tui.select.cancel") {
            return TrustSelectorAction::Cancel;
        }
        TrustSelectorAction::None
    }
}

fn format_trust_decision(
    trust_path: Option<&str>,
    decision: Option<&ProjectTrustStoreEntry>,
) -> String {
    let Some(decision) = decision else {
        return "none".to_string();
    };
    let label = if decision.decision {
        "trusted"
    } else {
        "untrusted"
    };
    match trust_path {
        Some(path) if path != decision.path => {
            format!("{label} (inherited from {})", decision.path)
        }
        _ => format!("{label} ({})", decision.path),
    }
}

impl Component for TrustSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let current_path = self
            .options
            .first()
            .and_then(|option| option.saved_path.as_deref());
        let mut lines = vec![
            t::bold(t::fg("accent", "Project trust")),
            self.cwd.clone(),
            format!(
                "Saved decision: {}",
                format_trust_decision(current_path, self.saved_decision.as_ref())
            ),
            format!(
                "Current session: {}",
                if self.project_trusted {
                    "trusted"
                } else {
                    "untrusted"
                }
            ),
            String::new(),
        ];
        for (index, option) in self.options.iter().enumerate() {
            let selected = index == self.selected;
            let saved = option.saved_path.as_deref().is_some_and(|path| {
                self.saved_decision.as_ref().is_some_and(|decision| {
                    decision.path == path && decision.decision == option.trusted
                })
            });
            let prefix = if selected { "→ " } else { "  " };
            let checkmark = if saved { " ✓" } else { "" };
            lines.push(format!("{prefix}{}{checkmark}", option.label));
        }
        lines.push(String::new());
        lines.push("↑/↓ navigate · Enter save · Esc cancel".to_string());
        lines
            .into_iter()
            .map(|line| pi_tui::utils::truncate_to_width(&line, width, "…"))
            .collect()
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
    model_selector_items_with_state(models, provider_filter, None, None)
}

/// Build model rows with the same current/default ordering and visible
/// labels as the pinned upstream model selector. The legacy builder above
/// remains source-compatible for callers that do not yet pass runtime state.
pub fn model_selector_items_with_state(
    models: &pi_ai::models::Models,
    provider_filter: Option<&str>,
    current_model: Option<&str>,
    default_model: Option<&str>,
) -> Vec<SelectItem> {
    let mut items = Vec::new();
    for model in models.get_available(provider_filter) {
        let pid = model.provider.clone();
        let mid = model.id.clone();
        let label = mid.clone();
        let value = format!("{pid}/{mid}");
        let mut description = format!("[{pid}] {} — {} context", model.name, model.context_window);
        if Some(value.as_str()) == default_model {
            description.push_str(" · default");
        }
        if Some(value.as_str()) == current_model {
            description.push_str(" ✓");
        }
        items.push(SelectItem {
            value,
            label,
            description: Some(description),
        });
    }
    prioritize_model_items(&mut items, current_model, default_model);
    items
}

/// Sort model rows by current model, default model, then provider and id.
/// This is separate from catalog construction so tests and future interactive
/// callers can prove the priority independently of network/catalog state.
pub fn prioritize_model_items(
    items: &mut [SelectItem],
    current_model: Option<&str>,
    default_model: Option<&str>,
) {
    items.sort_by(|left, right| {
        let left_current = Some(left.value.as_str()) == current_model;
        let right_current = Some(right.value.as_str()) == current_model;
        if left_current != right_current {
            return right_current.cmp(&left_current);
        }

        let left_default = Some(left.value.as_str()) == default_model;
        let right_default = Some(right.value.as_str()) == default_model;
        if left_default != right_default {
            return right_default.cmp(&left_default);
        }

        let left_provider = left
            .value
            .split_once('/')
            .map_or("", |(provider, _)| provider);
        let right_provider = right
            .value
            .split_once('/')
            .map_or("", |(provider, _)| provider);
        left_provider
            .cmp(right_provider)
            .then_with(|| left.value.cmp(&right.value))
    });
}

/// Fuzzy-filter model items by a query (slash argument completion).
pub fn filter_model_items(items: &[SelectItem], query: &str) -> Vec<SelectItem> {
    fuzzy_filter(items.to_vec(), query, |item| model_search_text(item, true))
}

/// Build the search text used by the upstream model selector. The provider
/// prefix comes first so an exact provider query outranks a proxy model ID;
/// the visible label and description remain searchable as well.
fn model_search_text(item: &SelectItem, bare_id_first: bool) -> String {
    let Some((provider, id)) = item.value.split_once('/') else {
        return format!(
            "{} {} {}",
            item.value,
            item.label,
            item.description.as_deref().unwrap_or_default()
        );
    };
    let description = item.description.as_deref().unwrap_or_default();
    if bare_id_first {
        format!("{id} {provider} {provider}/{id} {provider} {id}{description}")
    } else {
        format!("{provider} {provider}/{id} {provider} {id}{description}")
    }
}

fn thinking_level_description(level: &str) -> Option<&'static str> {
    THINKING_LEVEL_DESCRIPTIONS
        .iter()
        .find(|(candidate, _)| *candidate == level)
        .map(|(_, description)| *description)
}

/// Thinking level selector using the pinned upstream labels and descriptions.
pub fn thinking_selector_items() -> Vec<SelectItem> {
    thinking_selector_items_for(THINKING_LEVELS)
}

/// Build a thinking selector for a model's supported subset. Unknown or
/// duplicate levels are ignored, and the canonical upstream order is always
/// retained regardless of provider metadata ordering.
pub fn thinking_selector_items_for(levels: &[&str]) -> Vec<SelectItem> {
    THINKING_LEVELS
        .iter()
        .filter(|level| levels.contains(level))
        .map(|level| SelectItem {
            value: (*level).to_string(),
            label: (*level).to_string(),
            description: thinking_level_description(level).map(str::to_string),
        })
        .collect()
}

/// Convenience adapter for pi-ai's typed supported-level list.
pub fn thinking_selector_items_for_model(
    levels: &[pi_ai::types::ModelThinkingLevel],
) -> Vec<SelectItem> {
    let names = levels
        .iter()
        .map(|level| level.as_str())
        .collect::<Vec<_>>();
    thinking_selector_items_for(&names)
}

/// Build theme rows with the current theme marked exactly as upstream does.
/// `theme_selector_items` remains the compatibility builder for callers that
/// do not yet provide the active theme.
pub fn theme_selector_items_for(_current_theme: &str) -> Vec<SelectItem> {
    crate::theme::available_theme_names()
        .into_iter()
        .map(|name| SelectItem::new(name.clone(), name, None))
        .collect()
}

/// Theme selector backed by builtin, custom, and registered extension themes.
pub fn theme_selector_items() -> Vec<SelectItem> {
    crate::theme::available_theme_names()
        .into_iter()
        .map(|name| SelectItem::new(name.clone(), name, None))
        .collect()
}

fn setting_submenu_lines(
    title: &str,
    description: &str,
    search: Option<&Input>,
    min_primary_width: usize,
    max_primary_width: usize,
    rows: impl IntoIterator<Item = (bool, String, Option<String>)>,
    width: usize,
) -> Vec<String> {
    let mut lines = vec![t::bold(t::fg("accent", title))];
    if !description.is_empty() {
        lines.push(String::new());
        lines.extend(
            wrap_text_with_ansi(description, width.saturating_sub(4))
                .into_iter()
                .map(|line| t::fg("muted", line)),
        );
    }
    lines.push(String::new());
    if let Some(search) = search {
        lines.extend(search.render(width));
        lines.push(String::new());
    }

    let rows = rows.into_iter().collect::<Vec<_>>();
    if rows.is_empty() {
        lines.push(t::fg("dim", "  No matching settings"));
    } else {
        let selected_index = rows
            .iter()
            .position(|(selected, _, _)| *selected)
            .unwrap_or(0);
        let start = selected_index
            .saturating_sub(5)
            .min(rows.len().saturating_sub(10));
        let end = (start + 10).min(rows.len());
        let primary_column_width = rows
            .iter()
            .map(|(_, label, _)| visible_width(label))
            .max()
            .unwrap_or(0)
            .saturating_add(2)
            .clamp(min_primary_width, max_primary_width);
        for (selected, label, row_description) in rows[start..end].iter() {
            let prefix = if *selected { "→ " } else { "  " };
            let padded = format!(
                "{}{}",
                label,
                " ".repeat(primary_column_width.saturating_sub(visible_width(label)))
            );
            let mut row = format!("{prefix}{padded}");
            if let Some(row_description) = row_description {
                let available = width.saturating_sub(visible_width(&row) + 2);
                if available > 10 {
                    row.push_str(&truncate_to_width(row_description, available, ""));
                }
            }
            let row = truncate_to_width(&row, width, "");
            lines.push(if *selected { t::fg("accent", row) } else { row });
        }
        if start > 0 || end < rows.len() {
            lines.push(t::fg(
                "dim",
                format!("  ({}/{})", selected_index + 1, rows.len()),
            ));
        }
    }
    lines.push(String::new());
    lines.push(t::fg(
        "dim",
        if search.is_some() {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        },
    ));
    lines
}

/// Warning submenu backed by the same SettingsList primitive as upstream.
/// Changes are emitted immediately while the submenu remains open; Escape
/// only returns to the parent settings list.
struct WarningSettingsSubmenu {
    list: SettingsList,
    done: Option<SettingsSubmenuDoneFn>,
}

impl WarningSettingsSubmenu {
    fn new(current: &str, done: SettingsSubmenuDoneFn, on_change: SettingsSubmenuChangeFn) -> Self {
        let enabled = current
            .split_once('=')
            .and_then(|(_, value)| value.parse::<bool>().ok())
            .unwrap_or(true);
        let list = SettingsList::new_with_callbacks(
            vec![SettingItem::new(
                "anthropic-extra-usage",
                "Anthropic extra usage",
                if enabled { "true" } else { "false" },
                vec!["true".to_string(), "false".to_string()],
            )
            .with_description("Warn when Anthropic subscription auth may use paid extra usage")],
            10,
            settings_theme(),
            move |_, value| {
                on_change(format!("anthropic-extra-usage={value}"));
            },
            || {},
            SettingsListOptions::default(),
        );
        Self {
            list,
            done: Some(done),
        }
    }

    fn finish(&mut self) {
        if let Some(done) = self.done.take() {
            done(None, None);
        }
    }
}

impl Component for WarningSettingsSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        self.list.render(width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let is_cancel = get_keybindings().matches(key, "tui.select.cancel");
        self.list.handle_input(key);
        if is_cancel {
            self.finish();
        }
    }

    fn invalidate(&mut self) {
        self.list.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.list.set_focused(focused);
    }
}

#[derive(Debug, Clone)]
struct ModelThinkingChoice {
    key: String,
    label: String,
    description: Option<String>,
    model: Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelThinkingStage {
    Models,
    Levels(usize),
}

/// The settings-screen two-step per-model thinking selector. It deliberately
/// keeps the final persistence payload compact (`provider/model=level`) so the
/// settings panel can remain generic while the interactive owner performs the
/// real SettingsManager/session update.
struct ModelThinkingSubmenu {
    models: Vec<ModelThinkingChoice>,
    overrides: std::collections::BTreeMap<String, String>,
    current_model: Option<String>,
    default_model: Option<String>,
    global_thinking_level: String,
    stage: ModelThinkingStage,
    filtered_indices: Vec<usize>,
    level_choices: Vec<(String, String, Option<String>)>,
    selected_index: usize,
    search: Input,
    done: Option<SettingsSubmenuDoneFn>,
    on_change: Option<SettingsSubmenuChangeFn>,
}

impl std::fmt::Debug for ModelThinkingSubmenu {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelThinkingSubmenu")
            .field("stage", &self.stage)
            .field("models", &self.models)
            .field("selected_index", &self.selected_index)
            .finish()
    }
}

impl ModelThinkingSubmenu {
    fn model_key(model: &Model) -> String {
        format!("{}/{}", model.provider, model.id)
    }

    #[cfg(test)]
    fn new_with_global(
        models: Vec<Model>,
        current_model: Option<String>,
        default_model: Option<String>,
        overrides: std::collections::BTreeMap<String, String>,
        global_thinking_level: &str,
        done: SettingsSubmenuDoneFn,
    ) -> Self {
        let done_for_change = done;
        Self::new_with_global_callbacks(
            models,
            current_model,
            default_model,
            overrides,
            global_thinking_level,
            Box::new(|_, _| {}),
            Box::new(move |value| done_for_change(Some(value), None)),
        )
    }

    fn new_with_global_callbacks(
        mut models: Vec<Model>,
        current_model: Option<String>,
        default_model: Option<String>,
        overrides: std::collections::BTreeMap<String, String>,
        global_thinking_level: &str,
        done: SettingsSubmenuDoneFn,
        on_change: SettingsSubmenuChangeFn,
    ) -> Self {
        models.sort_by(|left, right| {
            let left_key = Self::model_key(left);
            let right_key = Self::model_key(right);
            let left_current = Some(left_key.as_str()) == current_model.as_deref();
            let right_current = Some(right_key.as_str()) == current_model.as_deref();
            right_current
                .cmp(&left_current)
                .then_with(|| {
                    let left_default = Some(left_key.as_str()) == default_model.as_deref();
                    let right_default = Some(right_key.as_str()) == default_model.as_deref();
                    right_default.cmp(&left_default)
                })
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.id.cmp(&right.id))
        });
        models.dedup_by(|left, right| left.provider == right.provider && left.id == right.id);
        let models = models
            .into_iter()
            .map(|model| {
                let key = Self::model_key(&model);
                let description = overrides.get(&key).cloned();
                ModelThinkingChoice {
                    label: format!("{} [{}]", model.id, model.provider),
                    key,
                    description,
                    model,
                }
            })
            .collect::<Vec<_>>();
        let filtered_indices = (0..models.len()).collect();
        let mut submenu = Self {
            models,
            overrides,
            current_model,
            default_model,
            global_thinking_level: global_thinking_level.to_string(),
            stage: ModelThinkingStage::Models,
            filtered_indices,
            level_choices: Vec::new(),
            selected_index: 0,
            search: Input::new("> "),
            done: Some(done),
            on_change: Some(on_change),
        };
        submenu.select_preferred_model();
        submenu
    }

    fn select_preferred_model(&mut self) {
        let preferred = self
            .current_model
            .as_deref()
            .or(self.default_model.as_deref());
        self.selected_index = preferred
            .and_then(|preferred| {
                self.filtered_indices.iter().position(|index| {
                    self.models
                        .get(*index)
                        .is_some_and(|model| model.key == preferred)
                })
            })
            .unwrap_or(0);
    }

    fn apply_filter(&mut self) {
        let query = self.search.value.clone();
        match self.stage {
            ModelThinkingStage::Models => {
                let indices = (0..self.models.len()).collect::<Vec<_>>();
                self.filtered_indices = fuzzy_filter(indices, &query, |index| {
                    let model = &self.models[*index];
                    format!(
                        "{} {} {} {}",
                        model.key,
                        model.label,
                        model.model.name,
                        model.description.as_deref().unwrap_or_default()
                    )
                });
            }
            ModelThinkingStage::Levels(_) => {
                let indices = (0..self.level_choices.len()).collect::<Vec<_>>();
                self.filtered_indices = fuzzy_filter(indices, &query, |index| {
                    let (_, label, description) = &self.level_choices[*index];
                    format!("{} {}", label, description.as_deref().unwrap_or_default())
                });
            }
        }
        self.selected_index = 0;
    }

    fn open_levels(&mut self, model_index: usize) {
        self.stage = ModelThinkingStage::Levels(model_index);
        self.level_choices.clear();
        if let Some(model) = self.models.get(model_index) {
            let supported_levels = if model.model.reasoning {
                pi_ai::model::get_supported_thinking_levels(&model.model)
            } else {
                vec![pi_ai::types::ModelThinkingLevel::Off]
            };
            self.level_choices = supported_levels
                .into_iter()
                .map(|level| {
                    let value = level.as_str().to_string();
                    let description = THINKING_LEVEL_DESCRIPTIONS
                        .iter()
                        .find(|(name, _)| *name == value)
                        .map(|(_, description)| (*description).to_string());
                    (value.clone(), value, description)
                })
                .collect();
            if self.overrides.contains_key(&model.key) {
                self.level_choices.push((
                    "__clear__".to_string(),
                    "(clear override)".to_string(),
                    Some(format!(
                        "Revert to global default ({})",
                        self.global_thinking_level
                    )),
                ));
            }
        }
        self.search.clear();
        self.filtered_indices = (0..self.level_choices.len()).collect();
        self.selected_index = self
            .models
            .get(model_index)
            .and_then(|model| self.overrides.get(&model.key))
            .and_then(|override_level| {
                self.level_choices
                    .iter()
                    .position(|(value, _, _)| value == override_level)
            })
            .unwrap_or(0);
    }

    fn finish(&mut self, value: Option<String>) {
        if let Some(done) = self.done.take() {
            done(value, None);
        }
    }

    fn move_selection(&mut self, direction: isize, steps: usize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        let step = direction.signum();
        if step == 0 {
            return;
        }
        for _ in 0..steps.max(1) {
            self.selected_index =
                (self.selected_index as isize + step).rem_euclid(len as isize) as usize;
        }
    }

    fn current_rows(&self) -> Vec<(bool, String, Option<String>)> {
        match self.stage {
            ModelThinkingStage::Models => {
                if self.models.is_empty() {
                    vec![(
                        true,
                        "No models available".to_string(),
                        Some("Log in to a provider or configure an API key first".to_string()),
                    )]
                } else {
                    self.filtered_indices
                        .iter()
                        .enumerate()
                        .map(|(display_index, index)| {
                            let model = &self.models[*index];
                            (
                                display_index == self.selected_index,
                                model.label.clone(),
                                model.description.clone(),
                            )
                        })
                        .collect()
                }
            }
            ModelThinkingStage::Levels(_) => self
                .filtered_indices
                .iter()
                .enumerate()
                .map(|(display_index, index)| {
                    let (_, label, description) = &self.level_choices[*index];
                    (
                        display_index == self.selected_index,
                        label.clone(),
                        description.clone(),
                    )
                })
                .collect(),
        }
    }
}

impl Component for ModelThinkingSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        let (title, description) = match self.stage {
            ModelThinkingStage::Models => (
                "Per-Model Thinking Level".to_string(),
                "Step 1/2 · Select a model to configure".to_string(),
            ),
            ModelThinkingStage::Levels(index) => {
                let model = self.models.get(index);
                (
                    format!(
                        "Thinking Level for {}",
                        model
                            .map(|model| model.label.clone())
                            .unwrap_or_else(|| "model".to_string())
                    ),
                    "Step 2/2 · Select default thinking level for this model".to_string(),
                )
            }
        };
        setting_submenu_lines(
            &title,
            &description,
            Some(&self.search),
            12,
            46,
            self.current_rows(),
            width,
        )
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let bindings = get_keybindings();
        if bindings.matches(key, "tui.select.cancel") {
            if matches!(self.stage, ModelThinkingStage::Levels(_)) {
                self.stage = ModelThinkingStage::Models;
                self.search.clear();
                self.filtered_indices = (0..self.models.len()).collect();
                self.select_preferred_model();
            } else {
                self.finish(None);
            }
            return;
        }
        if bindings.matches(key, "tui.select.up") {
            self.move_selection(-1, 1);
            return;
        }
        if bindings.matches(key, "tui.select.down") {
            self.move_selection(1, 1);
            return;
        }
        if bindings.matches(key, "tui.select.pageUp") {
            self.move_selection(-1, 10);
            return;
        }
        if bindings.matches(key, "tui.select.pageDown") {
            self.move_selection(1, 10);
            return;
        }
        if bindings.matches(key, "tui.select.confirm") {
            match self.stage {
                ModelThinkingStage::Models => {
                    if let Some(model_index) =
                        self.filtered_indices.get(self.selected_index).copied()
                    {
                        self.open_levels(model_index);
                    }
                }
                ModelThinkingStage::Levels(model_index) => {
                    let level = self
                        .filtered_indices
                        .get(self.selected_index)
                        .and_then(|index| self.level_choices.get(*index))
                        .map(|(level, _, _)| level.clone());
                    let model_key = self.models.get(model_index).map(|model| model.key.clone());
                    if let (Some(level), Some(model_key)) = (level, model_key) {
                        let value = format!("{model_key}={level}");
                        if level == "__clear__" {
                            self.overrides.remove(&model_key);
                        } else {
                            self.overrides.insert(model_key, level.clone());
                        }
                        if let Some(on_change) = &self.on_change {
                            on_change(value);
                        }
                        // Pi's stepped submenu loops after applying a level,
                        // so another model can be configured without
                        // reopening /settings. The live callback above is
                        // consumed by SettingsList on the next input dispatch
                        // and persists the canonical provider/model=level
                        // payload.
                        self.stage = ModelThinkingStage::Models;
                        self.level_choices.clear();
                        self.search.clear();
                        self.filtered_indices = (0..self.models.len()).collect();
                        self.select_preferred_model();
                    }
                }
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeStage {
    Single,
    Automatic,
    Light,
    Dark,
}

/// Bordered settings theme selector with the same single/automatic split as
/// Pi. Automatic mode owns two nested theme choices and an explicit Apply row,
/// so Escape can return to the parent without persisting a preview.
struct ThemeSubmenu {
    available_themes: Vec<String>,
    terminal_theme: String,
    original_theme_setting: String,
    stage: ThemeStage,
    single_theme: String,
    light_theme: String,
    dark_theme: String,
    filtered_indices: Vec<usize>,
    selected_index: usize,
    done: Option<SettingsSubmenuDoneFn>,
    preview: SettingsSubmenuPreviewFn,
}

impl std::fmt::Debug for ThemeSubmenu {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThemeSubmenu")
            .field("stage", &self.stage)
            .field("single_theme", &self.single_theme)
            .field("light_theme", &self.light_theme)
            .field("dark_theme", &self.dark_theme)
            .finish()
    }
}

impl ThemeSubmenu {
    fn preferred(available: &[String], preferred: Option<&str>) -> String {
        preferred
            .filter(|value| available.iter().any(|theme| theme == *value))
            .map(str::to_string)
            .or_else(|| {
                available
                    .iter()
                    .find(|theme| theme.as_str() == crate::theme::DEFAULT_THEME)
                    .cloned()
            })
            .or_else(|| available.first().cloned())
            .unwrap_or_else(|| crate::theme::DEFAULT_THEME.to_string())
    }

    #[cfg(test)]
    fn new(
        current: &str,
        terminal_theme: String,
        available_themes: Vec<String>,
        done: SettingsSubmenuDoneFn,
    ) -> Self {
        Self::new_with_preview(
            current,
            terminal_theme,
            available_themes,
            done,
            Box::new(|_| {}),
        )
    }

    fn new_with_preview(
        current: &str,
        terminal_theme: String,
        available_themes: Vec<String>,
        done: SettingsSubmenuDoneFn,
        preview: SettingsSubmenuPreviewFn,
    ) -> Self {
        let auto = crate::theme::parse_auto_theme_setting(Some(current));
        let fixed = (!current.contains('/')).then_some(current);
        let light_theme = auto
            .as_ref()
            .map(|(light, _)| Self::preferred(&available_themes, Some(light)))
            .unwrap_or_else(|| Self::preferred(&available_themes, fixed));
        let dark_theme = auto
            .as_ref()
            .map(|(_, dark)| Self::preferred(&available_themes, Some(dark)))
            .unwrap_or_else(|| Self::preferred(&available_themes, fixed));
        let single_theme = Self::preferred(
            &available_themes,
            fixed.or_else(|| {
                Some(if terminal_theme == "light" {
                    light_theme.as_str()
                } else {
                    dark_theme.as_str()
                })
            }),
        );
        let stage = if auto.is_some() {
            ThemeStage::Automatic
        } else {
            ThemeStage::Single
        };
        let mut submenu = Self {
            available_themes,
            terminal_theme,
            original_theme_setting: current.to_string(),
            stage,
            single_theme,
            light_theme,
            dark_theme,
            filtered_indices: Vec::new(),
            selected_index: 0,
            done: Some(done),
            preview,
        };
        submenu.reset_filter();
        submenu
    }

    fn single_rows(&self) -> Vec<(String, String, Option<String>)> {
        let mut rows = vec![(
            "/".to_string(),
            "Automatic".to_string(),
            Some("Use separate themes for light and dark terminal appearance".to_string()),
        )];
        rows.extend(
            self.available_themes
                .iter()
                .cloned()
                .map(|theme| (theme.clone(), theme, None)),
        );
        rows
    }

    fn automatic_rows(&self) -> Vec<(String, String, Option<String>)> {
        vec![
            (
                "light-theme".to_string(),
                "Light theme".to_string(),
                Some("Theme to use in automatic mode when the terminal is light".to_string()),
            ),
            (
                "dark-theme".to_string(),
                "Dark theme".to_string(),
                Some("Theme to use in automatic mode when the terminal is dark".to_string()),
            ),
            (
                "apply".to_string(),
                "Apply".to_string(),
                Some("Save and go back".to_string()),
            ),
            (
                "single-mode".to_string(),
                "Change mode".to_string(),
                Some("Switch to one theme for light and dark".to_string()),
            ),
        ]
    }

    fn slot_rows(&self) -> Vec<(String, String, Option<String>)> {
        self.available_themes
            .iter()
            .cloned()
            .map(|theme| (theme.clone(), theme, None))
            .collect()
    }

    fn rows(&self) -> Vec<(String, String, Option<String>)> {
        match self.stage {
            ThemeStage::Single => self.single_rows(),
            ThemeStage::Automatic => self.automatic_rows(),
            ThemeStage::Light | ThemeStage::Dark => self.slot_rows(),
        }
    }

    fn reset_filter(&mut self) {
        let rows = self.rows();
        self.filtered_indices = (0..rows.len()).collect();
        self.selected_index = match self.stage {
            ThemeStage::Single => rows
                .iter()
                .position(|(value, _, _)| value == &self.single_theme)
                .unwrap_or(0),
            ThemeStage::Light => rows
                .iter()
                .position(|(value, _, _)| value == &self.light_theme)
                .unwrap_or(0),
            ThemeStage::Dark => rows
                .iter()
                .position(|(value, _, _)| value == &self.dark_theme)
                .unwrap_or(0),
            ThemeStage::Automatic => 0,
        };
    }

    fn finish(&mut self, value: Option<String>) {
        if let Some(done) = self.done.take() {
            done(value, None);
        }
    }

    fn move_selection(&mut self, direction: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        self.selected_index =
            (self.selected_index as isize + direction).rem_euclid(len as isize) as usize;
    }

    fn selected_row(&self) -> Option<(String, String, Option<String>)> {
        let rows = self.rows();
        self.filtered_indices
            .get(self.selected_index)
            .and_then(|index| rows.get(*index))
            .cloned()
    }

    fn automatic_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }

    fn preview_setting(&self, setting: &str) {
        let active = crate::theme::resolve_theme_setting(Some(setting), &self.terminal_theme)
            .unwrap_or_else(|| setting.to_string());
        (self.preview)(active);
    }

    fn preview_current_setting(&self) {
        let setting = match self.stage {
            ThemeStage::Single => self.single_theme.clone(),
            ThemeStage::Automatic | ThemeStage::Light | ThemeStage::Dark => {
                self.automatic_setting()
            }
        };
        self.preview_setting(&setting);
    }

    fn preview_selection(&self) {
        let Some((value, _, _)) = self.selected_row() else {
            return;
        };
        match self.stage {
            ThemeStage::Single if value == "/" => self.preview_setting(&self.automatic_setting()),
            ThemeStage::Single => self.preview_setting(&value),
            ThemeStage::Light => self.preview_setting(&format!("{value}/{}", self.dark_theme)),
            ThemeStage::Dark => self.preview_setting(&format!("{}/{}", self.light_theme, value)),
            ThemeStage::Automatic => {}
        }
    }
}

impl Component for ThemeSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        let (title, description) = match self.stage {
            ThemeStage::Single => (
                "Theme".to_string(),
                "Select a theme, or choose Automatic to follow terminal appearance.".to_string(),
            ),
            ThemeStage::Automatic => (
                "Automatic Theme".to_string(),
                "Choose themes for terminal light and dark appearance.\nLight/dark detection requires terminal support.".to_string(),
            ),
            ThemeStage::Light => (
                "Light Theme".to_string(),
                "Select the theme to use for light terminal appearance".to_string(),
            ),
            ThemeStage::Dark => (
                "Dark Theme".to_string(),
                "Select the theme to use for dark terminal appearance".to_string(),
            ),
        };
        let rows = self
            .filtered_indices
            .iter()
            .enumerate()
            .filter_map(|(display_index, index)| {
                self.rows().get(*index).map(|(_, label, description)| {
                    (
                        display_index == self.selected_index,
                        label.clone(),
                        description.clone(),
                    )
                })
            });
        setting_submenu_lines(&title, &description, None, 12, 32, rows, width)
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let bindings = get_keybindings();
        if bindings.matches(key, "tui.select.cancel") {
            match self.stage {
                ThemeStage::Light | ThemeStage::Dark => {
                    self.stage = ThemeStage::Automatic;
                    self.reset_filter();
                    self.preview_current_setting();
                }
                ThemeStage::Automatic | ThemeStage::Single => {
                    self.preview_setting(&self.original_theme_setting);
                    self.finish(None);
                }
            }
            return;
        }
        if bindings.matches(key, "tui.select.up") {
            self.move_selection(-1);
            self.preview_selection();
            return;
        }
        if bindings.matches(key, "tui.select.down") {
            self.move_selection(1);
            self.preview_selection();
            return;
        }
        if bindings.matches(key, "tui.select.pageUp") {
            for _ in 0..10 {
                self.move_selection(-1);
            }
            self.preview_selection();
            return;
        }
        if bindings.matches(key, "tui.select.pageDown") {
            for _ in 0..10 {
                self.move_selection(1);
            }
            self.preview_selection();
            return;
        }
        if bindings.matches(key, "tui.select.confirm") {
            let Some((value, _, _)) = self.selected_row() else {
                return;
            };
            match self.stage {
                ThemeStage::Single => {
                    if value == "/" {
                        self.stage = ThemeStage::Automatic;
                        self.preview_current_setting();
                        self.reset_filter();
                    } else {
                        self.single_theme = value.clone();
                        self.preview_setting(&value);
                        self.finish(Some(value));
                    }
                }
                ThemeStage::Automatic => match value.as_str() {
                    "light-theme" => {
                        self.stage = ThemeStage::Light;
                        self.reset_filter();
                    }
                    "dark-theme" => {
                        self.stage = ThemeStage::Dark;
                        self.reset_filter();
                    }
                    "apply" => {
                        self.preview_current_setting();
                        self.finish(Some(self.automatic_setting()));
                    }
                    "single-mode" => {
                        self.single_theme = if self.terminal_theme == "light" {
                            self.light_theme.clone()
                        } else {
                            self.dark_theme.clone()
                        };
                        self.preview_setting(&self.single_theme);
                        self.stage = ThemeStage::Single;
                        self.reset_filter();
                    }
                    _ => {}
                },
                ThemeStage::Light => {
                    self.light_theme = value;
                    self.preview_current_setting();
                    self.stage = ThemeStage::Automatic;
                    self.reset_filter();
                }
                ThemeStage::Dark => {
                    self.dark_theme = value;
                    self.preview_current_setting();
                    self.stage = ThemeStage::Automatic;
                    self.reset_filter();
                }
            }
        }
    }

    fn invalidate(&mut self) {}

    fn set_focused(&mut self, _focused: bool) {}
}

/// Settings items for the settings selector.
pub fn settings_selector_items(
    settings: &SettingsManager,
) -> Vec<crate::interactive::settings_panel::SettingEntry> {
    use crate::interactive::settings_panel::SettingEntry;
    let bool_value = |value: bool| if value { "true" } else { "false" }.to_string();
    let format_timeout = |timeout_ms: u64| match timeout_ms {
        30_000 => "30 sec".to_string(),
        60_000 => "1 min".to_string(),
        120_000 => "2 min".to_string(),
        300_000 => "5 min".to_string(),
        0 => "disabled".to_string(),
        value => format!("{} sec", value / 1000),
    };
    let default_project_trust = match settings.get_default_project_trust() {
        "always" => "Always trust",
        "never" => "Never trust",
        _ => "Ask",
    };
    let model_thinking_levels = settings.get_all_model_thinking_levels();
    let model_thinking_summary = if model_thinking_levels.is_empty() {
        "none".to_string()
    } else {
        format!("{} configured", model_thinking_levels.len())
    };
    let mut entries = Vec::with_capacity(31);

    entries.push(
        SettingEntry::cycle(
            "autocompact",
            "Auto-compact",
            bool_value(settings.get_compaction_enabled()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Automatically compact context when it gets too large"),
    );

    if pi_tui::terminal_image::get_capabilities().images.is_some() {
        entries.push(
            SettingEntry::cycle(
                "show-images",
                "Show images",
                bool_value(settings.get_show_images()),
                vec!["true".to_string(), "false".to_string()],
            )
            .describe("Render images inline in terminal"),
        );
        entries.push(
            SettingEntry::cycle(
                "image-width-cells",
                "Image width",
                settings.get_image_width_cells().to_string(),
                vec!["60".to_string(), "80".to_string(), "120".to_string()],
            )
            .describe("Preferred inline image width in terminal cells"),
        );
    }

    entries.extend([
        SettingEntry::cycle(
            "auto-resize-images",
            "Auto-resize images",
            bool_value(settings.get_image_auto_resize()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Resize large images to 2000x2000 max for better model compatibility"),
        SettingEntry::cycle(
            "block-images",
            "Block images",
            bool_value(settings.get_block_images()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Prevent images from being sent to LLM providers"),
        SettingEntry::cycle(
            "skill-commands",
            "Skill commands",
            bool_value(settings.get_enable_skill_commands()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Register skills as /skill:name commands"),
        SettingEntry::cycle(
            "show-hardware-cursor",
            "Show hardware cursor",
            bool_value(settings.get_show_hardware_cursor()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Show the terminal cursor while still positioning it for IME support"),
        SettingEntry::cycle(
            "editor-padding",
            "Editor padding",
            settings.get_editor_padding_x().to_string(),
            ["0", "1", "2", "3"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Horizontal padding for input editor (0-3)"),
        SettingEntry::cycle(
            "output-padding",
            "Output padding",
            settings.get_output_pad().to_string(),
            vec!["0".to_string(), "1".to_string()],
        )
        .describe("Horizontal padding for user messages, assistant messages, and thinking"),
        SettingEntry::cycle(
            "autocomplete-max-visible",
            "Autocomplete max items",
            settings.get_autocomplete_max_visible().to_string(),
            ["3", "5", "7", "10", "15", "20"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Max visible items in autocomplete dropdown (3-20)"),
        SettingEntry::cycle(
            "clear-on-shrink",
            "Clear on shrink",
            bool_value(settings.get_clear_on_shrink()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Clear empty rows when content shrinks (may cause flicker)"),
        SettingEntry::cycle(
            "terminal-progress",
            "Terminal progress",
            bool_value(settings.get_show_terminal_progress()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Show OSC 9;4 progress indicators in the terminal tab bar"),
        SettingEntry::cycle(
            "steering-mode",
            "Steering mode",
            settings.get_steering_mode().to_string(),
            vec!["one-at-a-time".to_string(), "all".to_string()],
        )
        .describe("Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."),
        SettingEntry::cycle(
            "follow-up-mode",
            "Follow-up mode",
            settings.get_follow_up_mode().to_string(),
            vec!["one-at-a-time".to_string(), "all".to_string()],
        )
        .describe("Alt+Enter queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."),
        SettingEntry::cycle(
            "transport",
            "Transport",
            settings.get_transport().to_string(),
            ["sse", "websocket", "websocket-cached", "auto"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Preferred transport for providers that support multiple transports"),
        SettingEntry::cycle(
            "http-idle-timeout",
            "HTTP idle timeout",
            format_timeout(settings.get_http_idle_timeout_ms().unwrap_or(300_000)),
            ["30 sec", "1 min", "2 min", "5 min", "disabled"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes."),
        SettingEntry::cycle(
            "hide-thinking",
            "Hide thinking",
            bool_value(settings.get_hide_thinking_block()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Hide thinking blocks in assistant responses"),
        SettingEntry::cycle(
            "mermaid-rendering",
            "Mermaid diagrams",
            settings.get_mermaid_rendering_mode().to_string(),
            ["off", "final", "streaming"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Render Mermaid code blocks as Unicode diagrams"),
        SettingEntry::cycle(
            "cache-miss-notices",
            "Cache miss notices",
            bool_value(settings.get_show_cache_miss_notices()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Show transcript notices for significant prompt-cache misses and compaction costs"),
        SettingEntry::cycle(
            "collapse-changelog",
            "Collapse changelog",
            bool_value(settings.get_collapse_changelog()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Show condensed changelog after updates"),
        SettingEntry::cycle(
            "quiet-startup",
            "Quiet startup",
            bool_value(settings.get_quiet_startup()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Disable verbose printing at startup"),
        SettingEntry::cycle(
            "install-telemetry",
            "Install telemetry",
            bool_value(settings.get_enable_install_telemetry()),
            vec!["true".to_string(), "false".to_string()],
        )
        .describe("Send an anonymous version/update ping after changelog-detected updates"),
        SettingEntry::cycle(
            "default-project-trust",
            "Default project trust",
            default_project_trust.to_string(),
            ["Ask", "Always trust", "Never trust"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Fallback behavior when no extension or saved trust decision decides project trust"),
        SettingEntry::cycle(
            "double-escape-action",
            "Double-escape action",
            settings.get_double_escape_action().to_string(),
            ["tree", "fork", "none"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Action when pressing Escape twice with empty editor"),
        SettingEntry::cycle(
            "tree-filter-mode",
            "Tree filter mode",
            settings.get_tree_filter_mode().to_string(),
            ["default", "no-tools", "user-only", "labeled-only", "all"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
        .describe("Default filter when opening /tree"),
        SettingEntry::cycle(
            "warnings",
            "Warnings",
            "configure".to_string(),
            vec!["configure".to_string()],
        )
        .describe("Enable or disable individual warnings"),
        SettingEntry::cycle(
            "model-thinking",
            "Default thinking level per model",
            model_thinking_summary,
            vec!["configure".to_string()],
        )
        .describe("Override the default thinking level for specific models. Shift+Tab cycles thinking levels in-session."),
        SettingEntry::cycle(
            "tui-mode",
            "TUI mode",
            settings.get_tui_mode().to_string(),
            vec!["regular".to_string(), "fullscreen".to_string()],
        )
        .describe("Interface layout; fullscreen mode is experimental"),
        SettingEntry::cycle(
            "fullscreen-exit-output",
            "Fullscreen exit output",
            settings.get_fullscreen_exit_output().to_string(),
            vec!["transcript".to_string(), "resume-hint".to_string()],
        )
        .describe("Print the transcript or only a session resume hint when exiting fullscreen mode"),
        SettingEntry::cycle(
            "fullscreen-scrollbar",
            "Fullscreen scrollbar",
            settings.get_fullscreen_scrollbar().to_string(),
            vec!["auto".to_string(), "always".to_string(), "hidden".to_string()],
        )
        .describe("Scrollbar behavior in fullscreen mode; has no effect in regular mode"),
        SettingEntry::info(
            "theme",
            "Theme",
            settings
                .get_theme_setting()
                .unwrap_or(crate::theme::DEFAULT_THEME)
                .to_string(),
        )
        .describe("Color theme for the interface"),
    ]);

    entries
}

/// Build the settings selector with the live model/theme context owned by an
/// interactive session. The compatibility builder above intentionally keeps
/// the small registry-only API used by discovery tests; the real TUI uses this
/// variant so warnings, per-model thinking, and Automatic themes are actual
/// nested selectors instead of placeholder cycle rows.
pub fn settings_selector_items_for_runtime(
    settings: &SettingsManager,
    models: &pi_ai::models::Models,
    current_provider: &str,
    current_model: &Model,
) -> Vec<crate::interactive::settings_panel::SettingEntry> {
    use crate::interactive::settings_panel::SettingEntry;

    let mut entries = settings_selector_items(settings);

    let warning_enabled = settings
        .get_warnings()
        .get("anthropic-extra-usage")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let warning_current = format!("anthropic-extra-usage={warning_enabled}");
    let warning_state = std::sync::Arc::new(std::sync::Mutex::new(warning_current));
    let warning_state_for_factory = warning_state.clone();
    let warning_entry = SettingEntry::info("warnings", "Warnings", "configure".to_string())
        .describe("Enable or disable individual warnings")
        .with_submenu_callbacks(move |_current, done, on_change| {
            let current = warning_state_for_factory
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let warning_state_for_change = warning_state_for_factory.clone();
            let on_change = Box::new(move |value: String| {
                *warning_state_for_change
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value.clone();
                on_change(value);
            });
            Some(Box::new(WarningSettingsSubmenu::new(
                &current, done, on_change,
            )))
        });

    let available_models = models.get_available(None);
    let current_key = format!("{}/{}", current_provider, current_model.id);
    let default_key = match (
        settings.get_default_provider(),
        settings.get_default_model(),
    ) {
        (Some(_provider), Some(model)) if model.contains('/') => Some(model.to_string()),
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (None, Some(model)) if model.contains('/') => Some(model.to_string()),
        _ => None,
    };
    let model_overrides = settings
        .get_all_model_thinking_levels()
        .into_iter()
        .filter_map(|(key, value)| value.as_str().map(|level| (key, level.to_string())))
        .collect::<std::collections::BTreeMap<_, _>>();
    let model_summary = if model_overrides.is_empty() {
        "none".to_string()
    } else {
        format!("{} configured", model_overrides.len())
    };
    let model_current_key = current_key.clone();
    let model_default_key = default_key.clone();
    let model_overrides_for_menu = model_overrides.clone();
    let global_thinking_level = settings
        .get_default_thinking_level()
        .unwrap_or("medium")
        .to_string();
    let model_entry = SettingEntry::info(
        "model-thinking",
        "Default thinking level per model",
        model_summary,
    )
    .describe(
        "Override the default thinking level for specific models. Shift+Tab cycles in-session.",
    )
    .with_submenu_callbacks(move |_current, done, on_change| {
        Some(Box::new(ModelThinkingSubmenu::new_with_global_callbacks(
            available_models.clone(),
            Some(model_current_key.clone()),
            model_default_key.clone(),
            model_overrides_for_menu.clone(),
            &global_thinking_level,
            done,
            on_change,
        )))
    });

    let current_theme = settings
        .get_theme_setting()
        .unwrap_or(crate::theme::DEFAULT_THEME)
        .to_string();
    let terminal_theme = crate::theme::default_theme();
    let available_themes = crate::theme::available_theme_names();
    let theme_entry = SettingEntry::info("theme", "Theme", current_theme.clone())
        .describe("Color theme for the interface")
        .with_submenu_preview(move |_current, done, preview| {
            Some(Box::new(ThemeSubmenu::new_with_preview(
                &current_theme,
                terminal_theme.clone(),
                available_themes.clone(),
                done,
                preview,
            )))
        });

    for entry in &mut entries {
        match entry.id.as_str() {
            "warnings" => *entry = warning_entry.clone(),
            "model-thinking" => *entry = model_entry.clone(),
            "theme" => *entry = theme_entry.clone(),
            _ => {}
        }
    }
    entries
}

/// Build autocomplete items for @-attachments is handled by the provider;
/// this returns slash-command items for the editor.
pub fn slash_command_items() -> Vec<AutocompleteItem> {
    crate::interactive::slash::command_autocomplete_items()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn theme_selector_includes_registered_extension_name() {
        let _lock = crate::theme::test_theme_registry_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = std::env::temp_dir().join(format!("pi-selector-theme-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extension.json");
        let name = format!("selector-extension-theme-{}", uuid::Uuid::new_v4());
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../data/themes/dark.json")).unwrap();
        value["name"] = serde_json::Value::String(name.clone());
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        crate::theme::register_theme_paths(&[path.to_string_lossy().into_owned()], Path::new("."));
        assert!(theme_selector_items().iter().any(|item| item.value == name));

        crate::theme::register_theme_paths(&[], Path::new("."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scoped_models_toggle_commits_canonical_values() {
        let items = vec![
            SelectItem::new(
                "openai/gpt-5.5",
                "openai/gpt-5.5",
                Some("OpenAI".to_string()),
            ),
            SelectItem::new(
                "anthropic/claude-sonnet",
                "anthropic/claude-sonnet",
                Some("Anthropic".to_string()),
            ),
        ];
        let mut selector = ScopedModelsSelector::new(items, &[]);
        assert!(selector.selected_models().is_empty());
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            ScopedModelsAction::Toggle {
                model: "openai/gpt-5.5".to_string(),
                enabled: true,
            }
        );
        assert_eq!(selector.selected_models(), vec!["openai/gpt-5.5"]);
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            ScopedModelsAction::Toggle {
                model: "openai/gpt-5.5".to_string(),
                enabled: false,
            }
        );
        assert!(selector.selected_models().is_empty());
        assert_eq!(
            selector.handle(&TuiKey::simple("escape")),
            ScopedModelsAction::Cancel
        );
    }

    #[test]
    fn trust_selector_navigates_parent_persists_choice_and_cancels() {
        let root = std::env::temp_dir().join(format!("pi-trust-selector-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        let root_dir = std::fs::canonicalize(root).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let root = root_dir.to_string_lossy().into_owned();

        let saved = ProjectTrustStoreEntry {
            path: cwd.clone(),
            decision: false,
        };
        let mut selector = TrustSelector::new(cwd.clone(), Some(saved), false);
        assert_eq!(selector.selected_index(), 2);
        let rendered = selector.render(200).join("\n");
        assert!(rendered.contains("Saved decision: untrusted"), "{rendered}");
        assert!(rendered.contains("Trust parent folder"), "{rendered}");

        assert_eq!(
            selector.handle(&TuiKey::simple("up")),
            TrustSelectorAction::None
        );
        assert_eq!(selector.selected_index(), 1);
        let TrustSelectorAction::Select(selection) = selector.handle(&TuiKey::simple("enter"))
        else {
            panic!("expected parent trust selection");
        };
        assert!(selection.trusted);
        assert_eq!(
            selection.updates,
            vec![
                ProjectTrustUpdate {
                    path: root.clone(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: cwd.clone(),
                    decision: None,
                },
            ]
        );

        let mut cancelled = TrustSelector::new(cwd, None, false);
        assert_eq!(
            cancelled.handle(&TuiKey::simple("escape")),
            TrustSelectorAction::Cancel
        );

        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn list_selector_supports_fuzzy_search_and_backspace() {
        let mut selector = ListSelector::new_slash_layout(
            vec![
                SelectItem::new(
                    "openai-codex/gpt-5.5",
                    "openai-codex/gpt-5.5",
                    Some("Codex OAuth".to_owned()),
                ),
                SelectItem::new(
                    "anthropic/claude-sonnet",
                    "anthropic/claude-sonnet",
                    Some("Anthropic".to_owned()),
                ),
            ],
            10,
        );

        for character in "codex".chars() {
            assert_eq!(
                selector.handle(&TuiKey::simple(character.to_string())),
                SelectorAction::None
            );
        }
        assert_eq!(selector.search_query(), "codex");
        assert_eq!(
            selector.selected_item().map(|item| item.value),
            Some("openai-codex/gpt-5.5".to_owned())
        );
        assert!(selector.render(80)[0].contains("Search: codex"));
        assert_eq!(
            selector.handle(&TuiKey::simple("backspace")),
            SelectorAction::None
        );
        assert_eq!(selector.search_query(), "code");
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            SelectorAction::Select(Some(0))
        );
    }

    #[test]
    fn list_selector_does_not_treat_named_navigation_as_search_text() {
        let mut selector = ListSelector::new(thinking_selector_items(), 6);
        let _ = selector.handle(&TuiKey::simple("down"));
        assert_eq!(selector.search_query(), "");
        assert_eq!(selector.selected_index(), 1);
    }

    #[test]
    fn list_selector_clear_search_restores_the_current_position() {
        let mut selector = ListSelector::new(
            vec![
                SelectItem::new("one", "One", None),
                SelectItem::new("two", "Two", None),
                SelectItem::new("three", "Three", None),
            ],
            6,
        );
        for character in "t".chars() {
            let _ = selector.handle(&TuiKey::simple(character.to_string()));
        }
        let _ = selector.handle(&TuiKey::simple("down"));
        assert_eq!(
            selector.selected_item().map(|item| item.value),
            Some("three".into())
        );
        let _ = selector.handle(&TuiKey::simple("backspace"));
        assert_eq!(selector.search_query(), "");
        assert_eq!(selector.selected_index(), 1);
        assert_eq!(
            selector.selected_item().map(|item| item.value),
            Some("two".into())
        );
    }

    #[test]
    fn model_thinking_submenu_is_two_stage_and_emits_canonical_value() {
        let mut model = Model::new(
            "reasoning-model",
            "Reasoning model",
            "openai-responses",
            "openai",
        );
        model.reasoning = true;
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_for_done = result.clone();
        let mut submenu = ModelThinkingSubmenu::new_with_global(
            vec![model],
            Some("openai/reasoning-model".to_string()),
            None,
            std::collections::BTreeMap::new(),
            "medium",
            Box::new(move |value, _| {
                *result_for_done
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value;
            }),
        );

        // The first Enter selects the model and opens its level step. A
        // single Down advances exactly one thinking level.
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(submenu.stage, ModelThinkingStage::Levels(0));
        submenu.handle_input(&TuiKey::simple("down"));
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(
            result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref(),
            Some("openai/reasoning-model=minimal")
        );
    }

    #[test]
    fn model_thinking_submenu_filters_typed_model_before_opening_levels() {
        let distractor = Model::new("other-1", "Other Model", "other", "other");
        let target = Model::new("faux-1", "Faux Model", "faux", "faux");
        let mut submenu = ModelThinkingSubmenu::new_with_global(
            vec![distractor, target],
            None,
            None,
            std::collections::BTreeMap::new(),
            "off",
            Box::new(|_, _| {}),
        );

        for character in "faux-1".chars() {
            submenu.handle_input(&TuiKey::simple(character.to_string()));
        }

        assert_eq!(submenu.filtered_indices.len(), 1);
        let rendered = pi_tui::strip_ansi_codes(&submenu.render(100).join("\n"));
        assert!(
            rendered.contains("> faux-1"),
            "rendered submenu: {rendered}"
        );
        assert!(
            rendered.contains("→ faux-1 [faux]"),
            "rendered submenu: {rendered}"
        );
        assert!(
            !rendered.contains("other-1 [faux]"),
            "rendered submenu: {rendered}"
        );

        submenu.handle_input(&TuiKey::simple("enter"));
        assert!(matches!(submenu.stage, ModelThinkingStage::Levels(_)));
        let levels = pi_tui::strip_ansi_codes(&submenu.render(100).join("\n"));
        assert!(
            levels.contains("Thinking Level for faux-1 [faux]") && levels.contains("Step 2/2"),
            "rendered levels submenu: {levels}"
        );
    }

    #[test]
    fn model_thinking_settings_choice_is_live_and_loops_to_model_step() {
        let mut model = Model::new(
            "reasoning-model",
            "Reasoning model",
            "openai-responses",
            "openai",
        );
        model.reasoning = true;
        let changed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let changed_for_callback = changed.clone();
        let mut submenu = ModelThinkingSubmenu::new_with_global_callbacks(
            vec![model],
            Some("openai/reasoning-model".to_string()),
            None,
            std::collections::BTreeMap::new(),
            "medium",
            Box::new(|value, _| panic!("looping submenu closed with {value:?}")),
            Box::new(move |value| {
                changed_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(value)
            }),
        );

        submenu.handle_input(&TuiKey::simple("enter"));
        submenu.handle_input(&TuiKey::simple("down"));
        submenu.handle_input(&TuiKey::simple("enter"));

        assert_eq!(
            changed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            ["openai/reasoning-model=minimal".to_string()]
        );
        assert_eq!(submenu.stage, ModelThinkingStage::Models);
        assert_eq!(
            submenu
                .overrides
                .get("openai/reasoning-model")
                .map(String::as_str),
            Some("minimal")
        );
    }

    #[test]
    fn model_thinking_empty_state_and_clear_use_upstream_contract() {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_for_done = result.clone();
        let empty = ModelThinkingSubmenu::new_with_global(
            Vec::new(),
            None,
            None,
            std::collections::BTreeMap::new(),
            "medium",
            Box::new(move |value, _| {
                *result_for_done
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value;
            }),
        );
        assert!(empty
            .render(80)
            .iter()
            .any(|line| line.contains("No models available")));

        let mut model = Model::new("reasoning-model", "Reasoning model", "openai", "openai");
        model.reasoning = true;
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("openai/reasoning-model".to_string(), "high".to_string());
        let mut submenu = ModelThinkingSubmenu::new_with_global(
            vec![model],
            Some("openai/reasoning-model".to_string()),
            None,
            overrides,
            "low",
            Box::new(|_, _| {}),
        );
        submenu.handle_input(&TuiKey::simple("enter"));
        assert!(submenu
            .render(100)
            .iter()
            .any(|line| line.contains("Revert to global default (low)")));
    }

    #[test]
    fn warning_submenu_uses_canonical_value_and_settings_row_layout() {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_for_done = result.clone();
        let changed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let changed_for_callback = changed.clone();
        let mut submenu = WarningSettingsSubmenu::new(
            "anthropic-extra-usage=true",
            Box::new(move |value, _| {
                *result_for_done
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value;
            }),
            Box::new(move |value| {
                *changed_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(value);
            }),
        );
        let lines = submenu.render(90);
        assert!(lines
            .iter()
            .any(|line| line.contains("Anthropic extra usage") && line.contains("true")));
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(
            changed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref(),
            Some("anthropic-extra-usage=false")
        );
        submenu.handle_input(&TuiKey::simple("escape"));
        assert!(result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none());
    }

    #[test]
    fn theme_submenu_supports_automatic_light_dark_selection_and_cancel() {
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result_for_done = result.clone();
        let mut submenu = ThemeSubmenu::new(
            "dark",
            "dark".to_string(),
            vec!["dark".to_string(), "light".to_string()],
            Box::new(move |value, _| {
                *result_for_done
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value;
            }),
        );

        // Automatic is the first row in the single-theme step.
        submenu.handle_input(&TuiKey::simple("up"));
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(submenu.stage, ThemeStage::Automatic);

        // Open the light-theme child, choose light, then apply the resulting
        // slash-separated automatic setting.
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(submenu.stage, ThemeStage::Light);
        submenu.handle_input(&TuiKey::simple("down"));
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(submenu.stage, ThemeStage::Automatic);
        submenu.handle_input(&TuiKey::simple("down"));
        submenu.handle_input(&TuiKey::simple("down"));
        submenu.handle_input(&TuiKey::simple("enter"));
        assert_eq!(
            result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref(),
            Some("light/dark")
        );

        // A fresh submenu cancels without emitting a setting.
        let cancelled = std::sync::Arc::new(std::sync::Mutex::new(Some("sentinel".to_string())));
        let cancelled_for_done = cancelled.clone();
        let mut submenu = ThemeSubmenu::new(
            "dark",
            "dark".to_string(),
            vec!["dark".to_string(), "light".to_string()],
            Box::new(move |value, _| {
                *cancelled_for_done
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value;
            }),
        );
        submenu.handle_input(&TuiKey::simple("esc"));
        assert!(cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none());
    }

    #[test]
    fn theme_submenu_previews_selection_and_restores_original_on_cancel() {
        let previews = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let previews_for_callback = previews.clone();
        let done = std::sync::Arc::new(std::sync::Mutex::new(None));
        let done_for_callback = done.clone();
        let mut submenu = ThemeSubmenu::new_with_preview(
            "dark",
            "dark".to_string(),
            vec!["dark".to_string(), "light".to_string()],
            Box::new(move |value, _| {
                *done_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = value;
            }),
            Box::new(move |value| {
                previews_for_callback
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(value);
            }),
        );

        // The current fixed theme starts selected. Moving to light previews
        // it, but does not persist it or close the submenu.
        submenu.handle_input(&TuiKey::simple("down"));
        assert_eq!(
            previews
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .last()
                .map(String::as_str),
            Some("light")
        );
        assert!(done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none());

        // Escape restores the original theme and returns without a setting
        // change, matching the upstream preview/cancel contract.
        submenu.handle_input(&TuiKey::simple("escape"));
        assert_eq!(
            previews
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .last()
                .map(String::as_str),
            Some("dark")
        );
        assert!(done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none());
    }
}
