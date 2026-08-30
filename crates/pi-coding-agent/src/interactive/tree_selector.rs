//! Session-tree selector for interactive mode.
//!
//! The pinned Pi selector is an entry-backed tree rather than a flat command
//! list.  This component keeps the durable session entries as its source of
//! truth, flattens their parent links for keyboard navigation, and renders a
//! compact bordered view without exposing the stored JSON envelope.

use std::collections::{HashMap, HashSet};

use pi_agent::session::types::Entry;
use pi_agent::types::AgentMessage;
use pi_ai::types::{ContentBlock, Message, StopReason, UserContentBody};
use pi_tui::keybindings::get_keybindings;
use pi_tui::keys::TuiKey;
use pi_tui::tui::Component;
use pi_tui::utils::truncate_to_width;
use serde_json::Value;

use crate::interactive::tui_theme as t;

/// Actions emitted by the `/tree` selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeSelectorAction {
    None,
    Select(String),
    Cancel,
}

/// The persisted `/tree` filter modes from Pi's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeFilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl TreeFilterMode {
    /// Parse the settings value, falling back to Pi's default for invalid
    /// or older configuration values.
    pub fn from_setting(value: &str) -> Self {
        match value {
            "no-tools" => Self::NoTools,
            "user-only" => Self::UserOnly,
            "labeled-only" => Self::LabeledOnly,
            "all" => Self::All,
            _ => Self::Default,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
struct TreeNode {
    entry: Entry,
    label: Option<String>,
    children: Vec<TreeNode>,
}

#[derive(Debug, Clone)]
struct FlatTreeRow {
    entry: Entry,
    label: Option<String>,
    depth: usize,
    is_last: bool,
    ancestor_continues: Vec<bool>,
}

#[derive(Debug, Clone)]
struct TreeToolCall {
    name: String,
    arguments: Value,
}

/// A real parent-linked session-entry selector.
pub struct TreeSelector {
    rows: Vec<FlatTreeRow>,
    filtered_indices: Vec<usize>,
    selected_index: usize,
    max_visible_lines: usize,
    current_leaf_id: Option<String>,
    active_path_ids: HashSet<String>,
    tool_call_map: HashMap<String, TreeToolCall>,
    search_query: String,
    filter_mode: TreeFilterMode,
    last_selected_id: Option<String>,
}

impl TreeSelector {
    /// Build a selector from the session's durable entries.
    pub fn new(
        entries: Vec<Entry>,
        labels: HashMap<String, String>,
        current_leaf_id: Option<String>,
        terminal_height: usize,
    ) -> Self {
        Self::new_with_filter_mode(
            entries,
            labels,
            current_leaf_id,
            terminal_height,
            TreeFilterMode::Default,
        )
    }

    /// Build a selector with the persisted Pi `/tree` filter mode.
    pub fn new_with_filter_mode(
        entries: Vec<Entry>,
        labels: HashMap<String, String>,
        current_leaf_id: Option<String>,
        terminal_height: usize,
        filter_mode: TreeFilterMode,
    ) -> Self {
        let roots = build_tree(&entries, &labels);
        let mut rows = Vec::new();
        for (index, root) in roots.iter().enumerate() {
            flatten_tree(root, 0, index + 1 == roots.len(), Vec::new(), &mut rows);
        }
        let active_path_ids = active_path(&rows, current_leaf_id.as_deref());
        let tool_call_map = collect_tool_calls(&entries);
        let initial_selected_id = current_leaf_id.clone();
        let mut selector = Self {
            rows,
            filtered_indices: Vec::new(),
            selected_index: 0,
            max_visible_lines: (terminal_height / 2).max(5),
            current_leaf_id,
            active_path_ids,
            tool_call_map,
            search_query: String::new(),
            filter_mode,
            last_selected_id: None,
        };
        selector.rebuild_filter(initial_selected_id.as_deref());
        selector
    }

    pub fn count(&self) -> usize {
        self.filtered_indices.len()
    }

    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    pub fn filter_mode(&self) -> TreeFilterMode {
        self.filter_mode
    }

    /// Change the filter while preserving the selected entry when it remains
    /// visible, matching the selector's settings-driven runtime API.
    pub fn set_filter_mode(&mut self, filter_mode: TreeFilterMode) {
        if self.filter_mode == filter_mode {
            return;
        }
        let preserve_id = self.selected_entry_id();
        self.filter_mode = filter_mode;
        self.rebuild_filter(preserve_id.as_deref());
    }

    pub fn selected_entry_id(&self) -> Option<String> {
        self.selected_row().map(|row| row.entry.id().to_string())
    }

    fn selected_row(&self) -> Option<&FlatTreeRow> {
        self.filtered_indices
            .get(self.selected_index)
            .and_then(|index| self.rows.get(*index))
    }

    /// Return the selected row or the nearest visible ancestor of a requested
    /// row. Upstream keeps the cursor on the same entry when possible, walks
    /// through hidden bookkeeping rows when a filter removes it, and falls
    /// back to the final visible row only when no ancestor is visible.
    fn nearest_visible_index(&self, requested_id: Option<&str>) -> Option<usize> {
        if self.filtered_indices.is_empty() {
            return None;
        }

        let parents = self
            .rows
            .iter()
            .map(|row| {
                (
                    row.entry.id().to_string(),
                    row.entry.parent_id().map(str::to_string),
                )
            })
            .collect::<HashMap<_, _>>();
        let visible = self
            .filtered_indices
            .iter()
            .enumerate()
            .map(|(index, row_index)| (self.rows[*row_index].entry.id(), index))
            .collect::<HashMap<_, _>>();
        let mut current = requested_id.map(str::to_string);
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) {
                break;
            }
            if let Some(index) = visible.get(id.as_str()) {
                return Some(*index);
            }
            current = parents.get(&id).cloned().flatten();
        }
        Some(self.filtered_indices.len() - 1)
    }

    fn rebuild_filter(&mut self, preserve_id: Option<&str>) {
        let requested_id = preserve_id
            .map(str::to_string)
            .or_else(|| self.last_selected_id.clone());
        let tokens = self
            .search_query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let filter_mode = self.filter_mode;
        let current_leaf_id = self.current_leaf_id.as_deref();
        self.filtered_indices = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                if !passes_tree_filter(row, filter_mode, current_leaf_id) {
                    return false;
                }
                if tokens.is_empty() {
                    return true;
                }
                let text = searchable_text(row, &self.tool_call_map).to_lowercase();
                tokens.iter().all(|token| text.contains(token))
            })
            .map(|(index, _)| index)
            .collect();

        if let Some(index) = self.nearest_visible_index(requested_id.as_deref()) {
            self.selected_index = index;
            self.last_selected_id = self
                .selected_row()
                .map(|row| row.entry.id().to_string())
                .or(requested_id);
        } else {
            // Preserve the last valid selection through an empty search/filter
            // result so clearing it returns to the same row.
            self.selected_index = 0;
            self.last_selected_id = requested_id;
        }
    }

    pub fn handle(&mut self, key: &TuiKey) -> TreeSelectorAction {
        let bindings = get_keybindings();
        if bindings.matches(key, "tui.select.up") {
            if !self.filtered_indices.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_indices.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
            return TreeSelectorAction::None;
        }
        if bindings.matches(key, "tui.select.down") {
            if !self.filtered_indices.is_empty() {
                self.selected_index = (self.selected_index + 1) % self.filtered_indices.len();
            }
            return TreeSelectorAction::None;
        }
        if bindings.matches(key, "tui.editor.cursorLeft")
            || bindings.matches(key, "tui.select.pageUp")
        {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible_lines);
            return TreeSelectorAction::None;
        }
        if bindings.matches(key, "tui.editor.cursorRight")
            || bindings.matches(key, "tui.select.pageDown")
        {
            if !self.filtered_indices.is_empty() {
                self.selected_index = (self.selected_index + self.max_visible_lines)
                    .min(self.filtered_indices.len() - 1);
            }
            return TreeSelectorAction::None;
        }
        if bindings.matches(key, "tui.select.confirm") {
            return self
                .selected_entry_id()
                .map(TreeSelectorAction::Select)
                .unwrap_or(TreeSelectorAction::None);
        }
        if bindings.matches(key, "tui.select.cancel") {
            if !self.search_query.is_empty() {
                let preserve_id = self.selected_entry_id();
                self.search_query.clear();
                self.rebuild_filter(preserve_id.as_deref());
                return TreeSelectorAction::None;
            }
            return TreeSelectorAction::Cancel;
        }
        if bindings.matches(key, "tui.editor.deleteCharBackward") {
            let preserve_id = self.selected_entry_id();
            if self.search_query.pop().is_some() {
                self.rebuild_filter(preserve_id.as_deref());
            }
            return TreeSelectorAction::None;
        }

        if let Some(filter_mode) = direct_filter_mode(key, self.filter_mode) {
            let preserve_id = self.selected_entry_id();
            self.filter_mode = filter_mode;
            self.rebuild_filter(preserve_id.as_deref());
            return TreeSelectorAction::None;
        }

        let is_control = key.ctrl
            || key.alt
            || key.super_key
            || key.base.chars().any(|ch| {
                let code = ch as u32;
                code < 0x20 || code == 0x7f || (0x80..=0x9f).contains(&code)
            });
        if !is_control && !key.base.is_empty() {
            let preserve_id = self.selected_entry_id();
            self.search_query.push_str(&key.base);
            self.rebuild_filter(preserve_id.as_deref());
        }
        TreeSelectorAction::None
    }
}

fn direct_filter_mode(key: &TuiKey, current: TreeFilterMode) -> Option<TreeFilterMode> {
    let is_ctrl = |base: &str| {
        key.ctrl && !key.shift && !key.alt && !key.super_key && key.base.eq_ignore_ascii_case(base)
    };
    let is_shift_ctrl = |base: &str| {
        key.ctrl && key.shift && !key.alt && !key.super_key && key.base.eq_ignore_ascii_case(base)
    };

    if is_ctrl("d") {
        return Some(TreeFilterMode::Default);
    }
    if is_ctrl("t") {
        return Some(if current == TreeFilterMode::NoTools {
            TreeFilterMode::Default
        } else {
            TreeFilterMode::NoTools
        });
    }
    if is_ctrl("u") {
        return Some(if current == TreeFilterMode::UserOnly {
            TreeFilterMode::Default
        } else {
            TreeFilterMode::UserOnly
        });
    }
    if is_ctrl("l") {
        return Some(if current == TreeFilterMode::LabeledOnly {
            TreeFilterMode::Default
        } else {
            TreeFilterMode::LabeledOnly
        });
    }
    if is_ctrl("a") {
        return Some(if current == TreeFilterMode::All {
            TreeFilterMode::Default
        } else {
            TreeFilterMode::All
        });
    }
    if is_ctrl("o") || is_shift_ctrl("o") {
        const MODES: [TreeFilterMode; 5] = [
            TreeFilterMode::Default,
            TreeFilterMode::NoTools,
            TreeFilterMode::UserOnly,
            TreeFilterMode::LabeledOnly,
            TreeFilterMode::All,
        ];
        let index = MODES.iter().position(|mode| *mode == current).unwrap_or(0);
        let next = if key.shift {
            (index + MODES.len() - 1) % MODES.len()
        } else {
            (index + 1) % MODES.len()
        };
        return Some(MODES[next]);
    }
    None
}

fn passes_tree_filter(
    row: &FlatTreeRow,
    filter_mode: TreeFilterMode,
    current_leaf_id: Option<&str>,
) -> bool {
    // Pi hides assistant messages that only contain tool calls unless the
    // entry is the current leaf or represents an error/aborted turn.
    if let Entry::Message {
        message: AgentMessage::Core(Message::Assistant(assistant)),
        ..
    } = &row.entry
    {
        let is_current_leaf = current_leaf_id == Some(row.entry.id());
        let has_text = assistant
            .content()
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text, .. } if !text.is_empty()));
        let is_error_or_aborted = assistant
            .stop_reason()
            .is_some_and(|reason| !matches!(reason, StopReason::Stop | StopReason::ToolUse));
        if !is_current_leaf && !has_text && !is_error_or_aborted {
            return false;
        }
    }

    let is_settings_entry = matches!(
        &row.entry,
        Entry::Custom { .. } | Entry::ModelChange { .. } | Entry::ThinkingLevel { .. }
    );
    match filter_mode {
        TreeFilterMode::Default => !is_settings_entry,
        TreeFilterMode::NoTools => {
            !is_settings_entry
                && !matches!(
                    &row.entry,
                    Entry::Message {
                        message: AgentMessage::Core(Message::ToolResult(_)),
                        ..
                    }
                )
        }
        TreeFilterMode::UserOnly => matches!(
            &row.entry,
            Entry::Message {
                message: AgentMessage::Core(Message::User(_)),
                ..
            }
        ),
        TreeFilterMode::LabeledOnly => row.label.is_some(),
        TreeFilterMode::All => true,
    }
}

impl Component for TreeSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let border = border_line(width);
        let mut lines = vec![
            String::new(),
            border.clone(),
            t::bold(truncate_to_width("  Session Tree", width, "…")),
            t::dim(truncate_to_width(
                "  ↑/↓ move · ←/→ page · Enter select · Esc cancel",
                width,
                "…",
            )),
            search_line(&self.search_query, width),
            border.clone(),
            String::new(),
        ];

        if self.filtered_indices.is_empty() {
            lines.push(t::fg(
                "muted",
                truncate_to_width("  No entries found", width, "…"),
            ));
            lines.push(t::fg(
                "muted",
                truncate_to_width(
                    &format!("  (0/0){}", self.filter_status_label()),
                    width,
                    "…",
                ),
            ));
        } else {
            let start = self
                .selected_index
                .saturating_sub(self.max_visible_lines / 2)
                .min(
                    self.filtered_indices
                        .len()
                        .saturating_sub(self.max_visible_lines),
                );
            let end = (start + self.max_visible_lines).min(self.filtered_indices.len());
            for (visible_index, row_index) in self.filtered_indices[start..end].iter().enumerate() {
                let absolute_index = start + visible_index;
                let row = &self.rows[*row_index];
                let cursor = if absolute_index == self.selected_index {
                    "› "
                } else {
                    "  "
                };
                let prefix = tree_prefix(row);
                let active = if self.active_path_ids.contains(row.entry.id()) {
                    t::fg("accent", "• ")
                } else {
                    String::new()
                };
                let label = row
                    .label
                    .as_deref()
                    .filter(|label| !label.trim().is_empty())
                    .map(|label| t::fg("warning", format!("[{label}] ")))
                    .unwrap_or_default();
                let body = format!(
                    "{}{}{}{}{}",
                    cursor,
                    t::fg("dim", prefix),
                    active,
                    label,
                    entry_display_text(&row.entry, &self.tool_call_map),
                );
                let rendered = if absolute_index == self.selected_index {
                    t::bg("selectedBg", t::fg("selectedText", body))
                } else {
                    body
                };
                lines.push(truncate_to_width(&rendered, width, "…"));
            }
            lines.push(t::fg(
                "muted",
                truncate_to_width(
                    &format!(
                        "  ({}/{}){}",
                        self.selected_index + 1,
                        self.filtered_indices.len(),
                        self.filter_status_label(),
                    ),
                    width,
                    "…",
                ),
            ));
        }
        lines.push(String::new());
        lines.push(border);
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

impl TreeSelector {
    fn filter_status_label(&self) -> &'static str {
        match self.filter_mode {
            TreeFilterMode::Default => "",
            TreeFilterMode::NoTools => " [no-tools]",
            TreeFilterMode::UserOnly => " [user]",
            TreeFilterMode::LabeledOnly => " [labeled]",
            TreeFilterMode::All => " [all]",
        }
    }
}

fn border_line(width: usize) -> String {
    if width == 0 {
        String::new()
    } else {
        t::fg("border", "─".repeat(width))
    }
}

fn search_line(query: &str, width: usize) -> String {
    let text = if query.is_empty() {
        "  Type to search:".to_string()
    } else {
        format!("  Type to search: {}", t::fg("accent", query))
    };
    t::fg("muted", truncate_to_width(&text, width, "…"))
}

fn build_tree(entries: &[Entry], labels: &HashMap<String, String>) -> Vec<TreeNode> {
    let mut by_id = HashMap::<String, Entry>::new();
    let mut ids = Vec::new();
    for entry in entries {
        let id = entry.id().to_string();
        if by_id.contains_key(&id) {
            continue;
        }
        ids.push(id.clone());
        by_id.insert(id, entry.clone());
    }

    let mut children = HashMap::<String, Vec<String>>::new();
    let mut roots = Vec::new();
    for id in &ids {
        let Some(entry) = by_id.get(id) else {
            continue;
        };
        match entry.parent_id() {
            Some(parent) if parent != id && by_id.contains_key(parent) => {
                children
                    .entry(parent.to_string())
                    .or_default()
                    .push(id.clone());
            }
            _ => roots.push(id.clone()),
        }
    }

    let mut visited = HashSet::new();
    let mut tree = roots
        .iter()
        .filter_map(|id| build_node(id, &by_id, &children, labels, &mut visited))
        .collect::<Vec<_>>();
    // A malformed/cyclic session should remain navigable rather than making
    // the selector silently lose every entry that was not reachable from a
    // root.
    for id in ids {
        if !visited.contains(&id) {
            if let Some(node) = build_node(&id, &by_id, &children, labels, &mut visited) {
                tree.push(node);
            }
        }
    }
    tree
}

fn build_node(
    id: &str,
    by_id: &HashMap<String, Entry>,
    children: &HashMap<String, Vec<String>>,
    labels: &HashMap<String, String>,
    visited: &mut HashSet<String>,
) -> Option<TreeNode> {
    if !visited.insert(id.to_string()) {
        return None;
    }
    let entry = by_id.get(id)?.clone();
    let child_nodes = children
        .get(id)
        .into_iter()
        .flatten()
        .filter_map(|child| build_node(child, by_id, children, labels, visited))
        .collect();
    Some(TreeNode {
        entry,
        label: labels.get(id).cloned(),
        children: child_nodes,
    })
}

fn flatten_tree(
    node: &TreeNode,
    depth: usize,
    is_last: bool,
    ancestors: Vec<bool>,
    output: &mut Vec<FlatTreeRow>,
) {
    output.push(FlatTreeRow {
        entry: node.entry.clone(),
        label: node.label.clone(),
        depth,
        is_last,
        ancestor_continues: ancestors.clone(),
    });
    for (index, child) in node.children.iter().enumerate() {
        let child_is_last = index + 1 == node.children.len();
        let mut child_ancestors = ancestors.clone();
        if depth > 0 {
            child_ancestors.push(!is_last);
        }
        flatten_tree(child, depth + 1, child_is_last, child_ancestors, output);
    }
}

fn active_path(rows: &[FlatTreeRow], leaf_id: Option<&str>) -> HashSet<String> {
    let mut parents = HashMap::new();
    for row in rows {
        parents.insert(
            row.entry.id().to_string(),
            row.entry.parent_id().map(str::to_string),
        );
    }
    let mut path = HashSet::new();
    let mut current = leaf_id.map(str::to_string);
    while let Some(id) = current {
        if !path.insert(id.clone()) {
            break;
        }
        current = parents.get(&id).cloned().flatten();
    }
    path
}

fn tree_prefix(row: &FlatTreeRow) -> String {
    let mut prefix = row
        .ancestor_continues
        .iter()
        .map(|continues| if *continues { "│  " } else { "   " })
        .collect::<String>();
    if row.depth > 0 {
        prefix.push_str(if row.is_last { "└─ " } else { "├─ " });
    }
    prefix
}

fn searchable_text(row: &FlatTreeRow, tool_call_map: &HashMap<String, TreeToolCall>) -> String {
    format!(
        "{} {} {}",
        row.entry.id(),
        row.label.as_deref().unwrap_or_default(),
        entry_display_plain(&row.entry, tool_call_map)
    )
}

fn entry_display_text(entry: &Entry, tool_call_map: &HashMap<String, TreeToolCall>) -> String {
    let text = entry_display_plain(entry, tool_call_map);
    match entry {
        Entry::Message {
            message: AgentMessage::Core(Message::User(_)),
            ..
        } => format!("{}{}", t::fg("accent", "user: "), text),
        Entry::Message {
            message: AgentMessage::Core(Message::Assistant(_)),
            ..
        } => format!("{}{}", t::fg("success", "assistant: "), text),
        Entry::Message {
            message: AgentMessage::Core(Message::ToolResult(_)),
            ..
        } => t::fg("muted", text),
        _ => t::fg("dim", text),
    }
}

fn entry_display_plain(entry: &Entry, tool_call_map: &HashMap<String, TreeToolCall>) -> String {
    let compact = |text: &str, limit: usize| {
        text.replace(['\n', '\t'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(limit)
            .collect::<String>()
    };
    match entry {
        Entry::Message { message, .. } => match message {
            AgentMessage::Core(Message::User(user)) => match user.content() {
                UserContentBody::String(text) => compact(text, 80),
                UserContentBody::Blocks(blocks) => compact(
                    &blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    80,
                ),
            },
            AgentMessage::Core(Message::Assistant(assistant)) => {
                let text = assistant
                    .content()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if text.trim().is_empty() {
                    if assistant.error_message().is_some() {
                        "assistant: error".to_string()
                    } else {
                        "(no content)".to_string()
                    }
                } else {
                    compact(&text, 80)
                }
            }
            AgentMessage::Core(Message::ToolResult(result)) => {
                if let Some(tool_call) = tool_call_map.get(result.tool_call_id()) {
                    return format_tool_call_summary(&tool_call.name, &tool_call.arguments);
                }
                let text = result
                    .content()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                let detail = compact(&text, 32);
                if detail.is_empty() {
                    format!("tool({})", result.tool_name())
                } else {
                    format!("tool({}) {detail}", result.tool_name())
                }
            }
            AgentMessage::Custom(_) => "custom message".to_string(),
        },
        Entry::ModelChange {
            provider, model_id, ..
        } => format!("[model: {provider}/{model_id}]"),
        Entry::ThinkingLevel { thinking_level, .. } => {
            format!("[thinking: {thinking_level}]")
        }
        Entry::ActiveTools {
            active_tool_names, ..
        } => format!("[tools: {}]", active_tool_names.join(", ")),
        Entry::Compaction {
            summary,
            tokens_before,
            ..
        } => format!(
            "[compaction: {}k] {}",
            tokens_before / 1000,
            compact(summary, 56)
        ),
        Entry::BranchSummary { summary, .. } => {
            format!("[branch summary] {}", compact(summary, 64))
        }
        Entry::Custom { custom_type, .. } => format!("[custom: {custom_type}]"),
    }
}

fn collect_tool_calls(entries: &[Entry]) -> HashMap<String, TreeToolCall> {
    let mut calls = HashMap::new();
    for entry in entries {
        let Entry::Message {
            message: AgentMessage::Core(Message::Assistant(assistant)),
            ..
        } = entry
        else {
            continue;
        };
        for block in assistant.content() {
            let ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } = block
            else {
                continue;
            };
            calls.entry(id.clone()).or_insert_with(|| TreeToolCall {
                name: name.clone(),
                arguments: arguments.clone(),
            });
        }
    }
    calls
}

/// Render the tool result's originating call, as upstream's tree selector
/// does. A result often contains only output, so showing `tool(read)` loses
/// the useful path and makes adjacent calls indistinguishable.
fn format_tool_call_summary(name: &str, arguments: &Value) -> String {
    let string_argument = |key: &str| {
        arguments
            .as_object()
            .and_then(|object| object.get(key))
            .and_then(Value::as_str)
    };
    let path = || {
        string_argument("path")
            .or_else(|| string_argument("file_path"))
            .map(shorten_tree_path)
            .unwrap_or_default()
    };

    match name {
        "read" => {
            let mut display = path();
            let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(1);
            if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                display.push_str(&format!(":{offset}-{}", offset + limit.saturating_sub(1)));
            } else if arguments.get("offset").is_some() {
                display.push_str(&format!(":{offset}"));
            }
            format!("[read: {display}]")
        }
        "write" | "edit" => format!("[{name}: {}]", path()),
        "bash" => {
            let command = string_argument("command").unwrap_or_default();
            let command = command
                .replace(['\n', '\t'], " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let suffix = if command.chars().count() > 50 {
                "..."
            } else {
                ""
            };
            let command = command.chars().take(50).collect::<String>();
            format!("[bash: {command}{suffix}]")
        }
        "grep" | "find" => {
            let pattern = string_argument("pattern").unwrap_or_default();
            format!("[{name}: {pattern} in {}]", {
                let value = path();
                if value.is_empty() {
                    ".".to_string()
                } else {
                    value
                }
            })
        }
        "ls" => {
            let value = path();
            let value = if value.is_empty() {
                ".".to_string()
            } else {
                value
            };
            format!("[ls: {value}]")
        }
        _ => {
            let args = arguments.to_string();
            let args = args.chars().take(40).collect::<String>();
            let suffix = if args.chars().count() < arguments.to_string().chars().count() {
                "..."
            } else {
                ""
            };
            format!("[{name}: {args}{suffix}]")
        }
    }
}

fn shorten_tree_path(path: &str) -> String {
    let Some(home) = crate::config::home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if path == home {
        "~".to_string()
    } else {
        path.strip_prefix(home.as_ref())
            .and_then(|suffix| suffix.strip_prefix('/'))
            .map(|suffix| format!("~/{suffix}"))
            .unwrap_or_else(|| path.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_tui::strip_ansi_codes;

    fn user_entry(id: &str, parent_id: Option<&str>, text: &str, seq: u64) -> Entry {
        Entry::Message {
            id: id.to_owned(),
            seq,
            parent_id: parent_id.map(str::to_owned),
            timestamp: seq,
            message: AgentMessage::Core(Message::User(pi_ai::types::UserContent::string(
                text, seq,
            ))),
            terminate: None,
        }
    }

    fn assistant_entry(id: &str, parent_id: Option<&str>, text: Option<&str>, seq: u64) -> Entry {
        let mut assistant = pi_ai::types::AssistantMessage::new().with_timestamp(seq);
        if let Some(text) = text {
            assistant.content_mut().push(ContentBlock::text(text));
        }
        Entry::Message {
            id: id.to_owned(),
            seq,
            parent_id: parent_id.map(str::to_owned),
            timestamp: seq,
            message: AgentMessage::Core(Message::Assistant(assistant)),
            terminate: None,
        }
    }

    fn assistant_tool_entry(id: &str, parent_id: Option<&str>, call_id: &str, seq: u64) -> Entry {
        let mut assistant = pi_ai::types::AssistantMessage::new().with_timestamp(seq);
        assistant.content_mut().push(ContentBlock::tool_call(
            call_id,
            "read",
            serde_json::json!({"path": "src/lib.rs", "offset": 3, "limit": 3}),
        ));
        Entry::Message {
            id: id.to_owned(),
            seq,
            parent_id: parent_id.map(str::to_owned),
            timestamp: seq,
            message: AgentMessage::Core(Message::Assistant(assistant)),
            terminate: None,
        }
    }

    fn tool_result_entry(id: &str, parent_id: Option<&str>, seq: u64) -> Entry {
        Entry::Message {
            id: id.to_owned(),
            seq,
            parent_id: parent_id.map(str::to_owned),
            timestamp: seq,
            message: AgentMessage::Core(Message::ToolResult(
                pi_ai::types::ToolResultMessage::text(id, "read", "tool output", false),
            )),
            terminate: None,
        }
    }

    fn custom_entry(id: &str, parent_id: Option<&str>, seq: u64) -> Entry {
        Entry::Custom {
            id: id.to_owned(),
            seq,
            parent_id: parent_id.map(str::to_owned),
            timestamp: seq,
            custom_type: "session_info".to_owned(),
            data: None,
        }
    }

    #[test]
    fn renders_parent_linked_entries_and_selects_current_leaf() {
        let mut selector = TreeSelector::new(
            vec![
                user_entry("root", None, "first prompt", 1),
                user_entry("child", Some("root"), "branch prompt", 2),
            ],
            HashMap::new(),
            Some("child".to_owned()),
            30,
        );
        let rendered = strip_ansi_codes(&selector.render(100).join("\n"));
        assert!(rendered.contains("Session Tree"));
        assert!(rendered.contains("Type to search:"));
        assert!(rendered.contains("first prompt"));
        assert!(rendered.contains("└─"));
        assert!(rendered.contains("• user: branch prompt"));
        assert!(rendered.contains("(2/2)"));
        assert_eq!(
            selector.handle(&TuiKey::simple("enter")),
            TreeSelectorAction::Select("child".to_owned())
        );
    }

    #[test]
    fn escape_clears_search_before_cancelling() {
        let mut selector = TreeSelector::new(
            vec![user_entry("root", None, "first prompt", 1)],
            HashMap::new(),
            Some("root".to_owned()),
            30,
        );
        selector.handle(&TuiKey::simple("z"));
        assert_eq!(selector.search_query(), "z");
        assert_eq!(
            selector.handle(&TuiKey::simple("escape")),
            TreeSelectorAction::None
        );
        assert!(selector.search_query().is_empty());
        assert_eq!(
            selector.handle(&TuiKey::simple("escape")),
            TreeSelectorAction::Cancel
        );
    }

    #[test]
    fn persisted_filter_modes_match_upstream_entry_visibility() {
        let entries = vec![
            user_entry("user", None, "prompt", 1),
            assistant_entry("assistant", Some("user"), Some("reply"), 2),
            tool_result_entry("tool", Some("assistant"), 3),
            custom_entry("custom", Some("tool"), 4),
            Entry::ModelChange {
                id: "model".to_owned(),
                seq: 5,
                parent_id: Some("custom".to_owned()),
                timestamp: 5,
                provider: "openai".to_owned(),
                model_id: "gpt-5".to_owned(),
            },
            Entry::ThinkingLevel {
                id: "thinking".to_owned(),
                seq: 6,
                parent_id: Some("model".to_owned()),
                timestamp: 6,
                thinking_level: "medium".to_owned(),
            },
            assistant_entry("tool-only", Some("thinking"), None, 7),
        ];
        let mut labels = HashMap::new();
        labels.insert("user".to_owned(), "important".to_owned());
        labels.insert("custom".to_owned(), "metadata".to_owned());

        let selector = |mode| {
            TreeSelector::new_with_filter_mode(entries.clone(), labels.clone(), None, 30, mode)
        };
        assert_eq!(selector(TreeFilterMode::Default).count(), 3);
        assert_eq!(selector(TreeFilterMode::NoTools).count(), 2);
        assert_eq!(selector(TreeFilterMode::UserOnly).count(), 1);
        assert_eq!(selector(TreeFilterMode::LabeledOnly).count(), 2);
        assert_eq!(selector(TreeFilterMode::All).count(), 6);
        assert_eq!(
            TreeFilterMode::from_setting("user-only").as_str(),
            "user-only"
        );
        assert_eq!(
            TreeFilterMode::from_setting("unknown"),
            TreeFilterMode::Default
        );
    }

    #[test]
    fn changing_filter_preserves_selected_entry_when_visible() {
        let mut selector = TreeSelector::new(
            vec![
                user_entry("root", None, "prompt", 1),
                assistant_entry("reply", Some("root"), Some("reply"), 2),
            ],
            HashMap::new(),
            Some("reply".to_owned()),
            30,
        );
        assert_eq!(selector.selected_entry_id().as_deref(), Some("reply"));
        selector.set_filter_mode(TreeFilterMode::UserOnly);
        assert_eq!(selector.filter_mode(), TreeFilterMode::UserOnly);
        assert_eq!(selector.selected_entry_id().as_deref(), Some("root"));
    }

    #[test]
    fn tool_result_rows_show_their_originating_call_summary() {
        let entries = vec![
            user_entry("user", None, "inspect the source", 1),
            assistant_tool_entry("assistant", Some("user"), "call-1", 2),
            Entry::Message {
                id: "result".to_owned(),
                seq: 3,
                parent_id: Some("assistant".to_owned()),
                timestamp: 3,
                message: AgentMessage::Core(Message::ToolResult(
                    pi_ai::types::ToolResultMessage::text(
                        "call-1",
                        "read",
                        "source contents",
                        false,
                    ),
                )),
                terminate: None,
            },
        ];
        let selector = TreeSelector::new(entries, HashMap::new(), Some("result".into()), 30);
        let rendered = strip_ansi_codes(&selector.render(120).join("\n"));

        assert!(rendered.contains("[read: src/lib.rs:3-5]"), "{rendered}");
        assert!(!rendered.contains("tool(read) source contents"));
    }
}
