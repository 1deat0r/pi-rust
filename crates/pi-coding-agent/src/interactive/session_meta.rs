//! Session-picker state and pure helpers.
//!
//! The pinned upstream session selector has two distinct layers: a metadata
//! loader and a keyboard/rendering component. The current Rust mode still
//! calls the small legacy SelectList adapter, so this module keeps that
//! adapter source-compatible while providing the complete deterministic
//! search, tree, and action state that the caller can adopt.

use std::collections::{HashMap, HashSet};

use pi_agent::session::types::SessionMetadata;
use pi_tui::components::select_list::SelectItem;
use pi_tui::components::Input;
use pi_tui::fuzzy::fuzzy_match;
use pi_tui::keybindings::get_keybindings;
use pi_tui::keys::TuiKey;
use pi_tui::tui::Component;
use pi_tui::utils::{truncate_to_width, visible_width};

use crate::interactive::tui_theme as t;

/// Minimal metadata carried by the legacy /resume picker.
#[derive(Debug, Clone)]
pub struct SessionMetaForPicker {
    pub id: String,
    pub label: String,
    pub metadata: SessionMetadata,
}

/// Rich session data needed by the upstream search and threaded display.
/// Header-only callers can construct this through session_picker_records;
/// loaders that read the JSONL body can fill in name/message fields exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPickerRecord {
    pub id: String,
    pub path: String,
    pub cwd: String,
    pub name: Option<String>,
    pub created_at: u64,
    pub modified_at: u64,
    pub message_count: usize,
    pub first_message: String,
    pub all_messages_text: String,
    pub parent_session_id: Option<String>,
    pub parent_session_path: Option<String>,
}

impl SessionPickerRecord {
    pub fn display_text(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                if self.first_message.trim().is_empty() {
                    &self.id
                } else {
                    &self.first_message
                }
            })
    }

    pub fn has_name(&self) -> bool {
        self.name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
    }
}

fn metadata_string(metadata: &SessionMetadata, key: &str) -> Option<String> {
    metadata
        .metadata
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn metadata_u64(metadata: &SessionMetadata, key: &str) -> Option<u64> {
    metadata
        .metadata
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_u64)
}

impl From<&SessionMetaForPicker> for SessionPickerRecord {
    fn from(item: &SessionMetaForPicker) -> Self {
        let metadata = &item.metadata;
        Self {
            id: item.id.clone(),
            path: metadata.path.clone(),
            cwd: metadata.cwd.clone(),
            name: metadata_string(metadata, "name"),
            created_at: metadata.created_at,
            modified_at: metadata.modified_at,
            message_count: metadata_u64(metadata, "messageCount")
                .or_else(|| metadata_u64(metadata, "message_count"))
                .unwrap_or(0) as usize,
            first_message: metadata_string(metadata, "firstMessage")
                .or_else(|| metadata_string(metadata, "first_message"))
                .unwrap_or_else(|| item.label.clone()),
            all_messages_text: metadata_string(metadata, "allMessagesText")
                .or_else(|| metadata_string(metadata, "all_messages_text"))
                .unwrap_or_else(|| item.label.clone()),
            parent_session_id: metadata.parent_session_id.clone(),
            parent_session_path: metadata.legacy_parent_session_path.clone(),
        }
    }
}

/// Sort sessions newest-first and render picker labels from file names.
pub fn session_picker_items(sessions: Vec<SessionMetadata>) -> Vec<SessionMetaForPicker> {
    let mut sessions = sessions;
    sessions.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions
        .into_iter()
        .map(|metadata| {
            let label = std::path::Path::new(&metadata.path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| metadata.id.clone());
            SessionMetaForPicker {
                id: metadata.id.clone(),
                label,
                metadata,
            }
        })
        .collect()
}

/// Convert legacy picker metadata into rich search/tree records.
pub fn session_picker_records(items: &[SessionMetaForPicker]) -> Vec<SessionPickerRecord> {
    items.iter().map(SessionPickerRecord::from).collect()
}

/// Build SelectItems for the legacy picker UI.
pub fn picker_select_items(
    items: &[SessionMetaForPicker],
) -> Vec<pi_tui::components::select_list::SelectItem> {
    items
        .iter()
        .map(|item| {
            SelectItem::new(
                item.id.clone(),
                item.label.clone(),
                Some(item.metadata.cwd.clone()),
            )
        })
        .collect()
}

/// Build startup-picker rows whose searchable description includes the full
/// session path.  The legacy `/resume` modal keeps its compact cwd summary,
/// while CLI `--resume` must also accept an exact path typed into the picker.
pub fn picker_select_items_with_paths(
    items: &[SessionMetaForPicker],
) -> Vec<pi_tui::components::select_list::SelectItem> {
    items
        .iter()
        .map(|item| {
            SelectItem::new(
                item.id.clone(),
                item.label.clone(),
                Some(item.metadata.path.clone()),
            )
        })
        .collect()
}

/// Session-list sorting mode from the pinned upstream component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSortMode {
    Threaded,
    Recent,
    Relevance,
}

/// Named-session filter from the pinned upstream component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionNameFilter {
    All,
    Named,
}

/// Current-folder/all-session scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    Current,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSearchTokenKind {
    Fuzzy,
    Phrase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchToken {
    pub kind: SessionSearchTokenKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSearchMode {
    Tokens,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSessionSearchQuery {
    pub mode: SessionSearchMode,
    pub tokens: Vec<SessionSearchToken>,
    pub regex: Option<String>,
    pub error: Option<String>,
}

fn push_search_token(
    tokens: &mut Vec<SessionSearchToken>,
    kind: SessionSearchTokenKind,
    buffer: &mut String,
) {
    let value = buffer.trim();
    if !value.is_empty() {
        tokens.push(SessionSearchToken {
            kind,
            value: value.to_string(),
        });
    }
    buffer.clear();
}

/// Parse upstream session search syntax: whitespace-separated fuzzy tokens,
/// quoted exact phrases, and case-insensitive re:<pattern> regex mode.
pub fn parse_session_search_query(query: &str) -> ParsedSessionSearchQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedSessionSearchQuery {
            mode: SessionSearchMode::Tokens,
            tokens: Vec::new(),
            regex: None,
            error: None,
        };
    }

    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedSessionSearchQuery {
                mode: SessionSearchMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some("Empty regex".to_string()),
            };
        }
        let error = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .err()
            .map(|error| error.to_string());
        return ParsedSessionSearchQuery {
            mode: SessionSearchMode::Regex,
            tokens: Vec::new(),
            regex: (error.is_none()).then(|| pattern.to_string()),
            error,
        };
    }

    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let mut in_quote = false;
    for character in trimmed.chars() {
        if character == '\"' {
            if in_quote {
                push_search_token(&mut tokens, SessionSearchTokenKind::Phrase, &mut buffer);
                in_quote = false;
            } else {
                push_search_token(&mut tokens, SessionSearchTokenKind::Fuzzy, &mut buffer);
                in_quote = true;
            }
        } else if !in_quote && character.is_whitespace() {
            push_search_token(&mut tokens, SessionSearchTokenKind::Fuzzy, &mut buffer);
        } else {
            buffer.push(character);
        }
    }

    if in_quote {
        // Match upstream's forgiving behavior: an unclosed quote falls back
        // to ordinary whitespace tokenization instead of returning an error.
        let tokens = trimmed
            .split_whitespace()
            .map(|value| SessionSearchToken {
                kind: SessionSearchTokenKind::Fuzzy,
                value: value.to_string(),
            })
            .collect();
        return ParsedSessionSearchQuery {
            mode: SessionSearchMode::Tokens,
            tokens,
            regex: None,
            error: None,
        };
    }

    push_search_token(&mut tokens, SessionSearchTokenKind::Fuzzy, &mut buffer);
    ParsedSessionSearchQuery {
        mode: SessionSearchMode::Tokens,
        tokens,
        regex: None,
        error: None,
    }
}

fn normalize_whitespace_lower(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Search corpus used by upstream session-selector-search.ts.
pub fn session_search_text(session: &SessionPickerRecord) -> String {
    format!(
        "{} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or_default(),
        session.all_messages_text,
        session.cwd
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMatchResult {
    pub matches: bool,
    pub score: f64,
}

/// Match one record using the parsed upstream query.
pub fn match_session(
    session: &SessionPickerRecord,
    parsed: &ParsedSessionSearchQuery,
) -> SessionMatchResult {
    let text = session_search_text(session);
    if parsed.mode == SessionSearchMode::Regex {
        let Some(pattern) = parsed.regex.as_deref() else {
            return SessionMatchResult {
                matches: false,
                score: 0.0,
            };
        };
        let Ok(regex) = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        else {
            return SessionMatchResult {
                matches: false,
                score: 0.0,
            };
        };
        return regex
            .find(&text)
            .map(|matched| SessionMatchResult {
                matches: true,
                score: matched.start() as f64 * 0.1,
            })
            .unwrap_or(SessionMatchResult {
                matches: false,
                score: 0.0,
            });
    }

    let mut score = 0.0;
    for token in &parsed.tokens {
        match token.kind {
            SessionSearchTokenKind::Phrase => {
                let normalized_text = normalize_whitespace_lower(&text);
                let phrase = normalize_whitespace_lower(&token.value);
                let Some(index) = normalized_text.find(&phrase) else {
                    return SessionMatchResult {
                        matches: false,
                        score: 0.0,
                    };
                };
                score += index as f64 * 0.1;
            }
            SessionSearchTokenKind::Fuzzy => {
                let result = fuzzy_match(&token.value, &text);
                if !result.matches {
                    return SessionMatchResult {
                        matches: false,
                        score: 0.0,
                    };
                }
                score += result.score;
            }
        }
    }
    SessionMatchResult {
        matches: true,
        score,
    }
}

/// Whether a session has a non-blank user-defined name.
pub fn has_session_name(session: &SessionPickerRecord) -> bool {
    session.has_name()
}

/// Filter and sort rich session records using upstream semantics.
pub fn filter_and_sort_sessions(
    sessions: &[SessionPickerRecord],
    query: &str,
    sort_mode: SessionSortMode,
    name_filter: SessionNameFilter,
) -> Vec<SessionPickerRecord> {
    let name_filtered = sessions
        .iter()
        .filter(|session| name_filter == SessionNameFilter::All || has_session_name(session))
        .cloned()
        .collect::<Vec<_>>();
    if query.trim().is_empty() {
        return name_filtered;
    }

    let parsed = parse_session_search_query(query);
    if parsed.error.is_some() {
        return Vec::new();
    }

    if sort_mode == SessionSortMode::Recent {
        return name_filtered
            .into_iter()
            .filter(|session| match_session(session, &parsed).matches)
            .collect();
    }

    let mut scored = name_filtered
        .into_iter()
        .filter_map(|session| {
            let result = match_session(&session, &parsed);
            result.matches.then_some((session, result.score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left, left_score), (right, right_score)| {
        left_score
            .partial_cmp(right_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.modified_at.cmp(&left.modified_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    scored.into_iter().map(|(session, _)| session).collect()
}

/// A threaded session node with latest descendant activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTreeNode {
    pub session: SessionPickerRecord,
    pub children: Vec<SessionTreeNode>,
    pub latest_activity: u64,
}

/// A flattened threaded row with enough information to draw tree branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatSessionNode {
    pub session: SessionPickerRecord,
    pub depth: usize,
    pub is_last: bool,
    pub ancestor_continues: Vec<bool>,
}

fn canonical_session_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let mut result = Vec::with_capacity(normalized.len());
    let absolute = normalized.starts_with('/');
    for component in normalized.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if !result.is_empty() && result.last().is_some_and(|part| part != "..") {
                    result.pop();
                } else if !absolute {
                    result.push("..".to_string());
                }
            }
            component => result.push(component.to_string()),
        }
    }
    let mut path = result.join("/");
    if absolute {
        path.insert(0, '/');
    }
    if path.is_empty() {
        if absolute {
            "/".to_string()
        } else {
            ".".to_string()
        }
    } else {
        path
    }
}

/// Compare project directories the way a user sees them, resolving existing
/// symlinks but retaining the lexical fallback for a deleted project.
pub fn session_cwds_match(left: &str, right: &str) -> bool {
    fn normalized(path: &str) -> String {
        std::fs::canonicalize(path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| canonical_session_path(path))
    }
    normalized(left) == normalized(right)
}

fn creates_parent_cycle(index: usize, parent: usize, parents: &[Option<usize>]) -> bool {
    let mut current = Some(parent);
    let mut seen = HashSet::new();
    while let Some(candidate) = current {
        if candidate == index || !seen.insert(candidate) {
            return true;
        }
        current = parents[candidate];
    }
    false
}

fn latest_activity(
    index: usize,
    records: &[SessionPickerRecord],
    children: &[Vec<usize>],
    memo: &mut [Option<u64>],
) -> u64 {
    if let Some(value) = memo[index] {
        return value;
    }
    let latest = children[index]
        .iter()
        .fold(records[index].modified_at, |latest, child| {
            latest.max(latest_activity(*child, records, children, memo))
        });
    memo[index] = Some(latest);
    latest
}

fn build_tree_node(
    index: usize,
    records: &[SessionPickerRecord],
    children: &[Vec<usize>],
    latest: &[u64],
) -> SessionTreeNode {
    let mut child_nodes = children[index]
        .iter()
        .map(|child| build_tree_node(*child, records, children, latest))
        .collect::<Vec<_>>();
    child_nodes.sort_by(|left, right| {
        right
            .latest_activity
            .cmp(&left.latest_activity)
            .then_with(|| right.session.modified_at.cmp(&left.session.modified_at))
            .then_with(|| left.session.id.cmp(&right.session.id))
    });
    SessionTreeNode {
        session: records[index].clone(),
        children: child_nodes,
        latest_activity: latest[index],
    }
}

/// Build a parent/child tree by session id, falling back to legacy parent
/// paths. Missing parents become roots and cyclic metadata is made safe.
pub fn build_session_tree(records: &[SessionPickerRecord]) -> Vec<SessionTreeNode> {
    let mut by_id = HashMap::new();
    let mut by_path = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        by_id.entry(record.id.clone()).or_insert(index);
        by_path
            .entry(canonical_session_path(&record.path))
            .or_insert(index);
    }

    let mut parents = vec![None; records.len()];
    for (index, record) in records.iter().enumerate() {
        let parent = record
            .parent_session_id
            .as_ref()
            .and_then(|id| by_id.get(id).copied())
            .or_else(|| {
                record
                    .parent_session_path
                    .as_deref()
                    .and_then(|path| by_path.get(&canonical_session_path(path)).copied())
            });
        if let Some(parent) = parent {
            if parent != index && !creates_parent_cycle(index, parent, &parents) {
                parents[index] = Some(parent);
            }
        }
    }

    let mut children = vec![Vec::new(); records.len()];
    let mut roots = Vec::new();
    for (index, parent) in parents.into_iter().enumerate() {
        if let Some(parent) = parent {
            children[parent].push(index);
        } else {
            roots.push(index);
        }
    }

    let mut memo = vec![None; records.len()];
    let latest = (0..records.len())
        .map(|index| latest_activity(index, records, &children, &mut memo))
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        latest[*right]
            .cmp(&latest[*left])
            .then_with(|| records[*right].modified_at.cmp(&records[*left].modified_at))
            .then_with(|| records[*left].id.cmp(&records[*right].id))
    });
    roots
        .into_iter()
        .map(|root| build_tree_node(root, records, &children, &latest))
        .collect()
}

/// Flatten a threaded tree in display order.
pub fn flatten_session_tree(roots: &[SessionTreeNode]) -> Vec<FlatSessionNode> {
    fn walk(
        node: &SessionTreeNode,
        depth: usize,
        ancestor_continues: Vec<bool>,
        is_last: bool,
        output: &mut Vec<FlatSessionNode>,
    ) {
        output.push(FlatSessionNode {
            session: node.session.clone(),
            depth,
            is_last,
            ancestor_continues: ancestor_continues.clone(),
        });
        for (index, child) in node.children.iter().enumerate() {
            let child_is_last = index + 1 == node.children.len();
            let continues = depth > 0 && !is_last;
            let mut child_ancestors = ancestor_continues.clone();
            child_ancestors.push(continues);
            walk(child, depth + 1, child_ancestors, child_is_last, output);
        }
    }

    let mut output = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        walk(root, 0, Vec::new(), index + 1 == roots.len(), &mut output);
    }
    output
}

/// Actions emitted by SessionPickerState. Mutation itself remains at the
/// caller boundary because deleting/renaming/forking touches session storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerAction {
    None,
    Select { id: String, path: String },
    Cancel,
    ScopeChanged(SessionScope),
    SortChanged(SessionSortMode),
    NameFilterChanged(SessionNameFilter),
    PathVisibilityChanged(bool),
    BeginRename(String),
    DeleteRequested(String),
    DeleteConfirmed(String),
    DeleteCancelled,
    DeleteCurrentDenied(String),
}

/// Deterministic session selector state. It mirrors the upstream SessionList
/// key precedence, including delete confirmation and search focus, without
/// performing storage mutations itself.
pub struct SessionPickerState {
    all_sessions: Vec<SessionPickerRecord>,
    filtered_sessions: Vec<FlatSessionNode>,
    current_cwd: String,
    current_session_path: Option<String>,
    scope: SessionScope,
    sort_mode: SessionSortMode,
    name_filter: SessionNameFilter,
    selected: usize,
    max_visible: usize,
    show_path: bool,
    confirming_delete: Option<String>,
    search: Input,
}

impl SessionPickerState {
    pub fn new(
        sessions: Vec<SessionPickerRecord>,
        current_cwd: impl Into<String>,
        current_session_path: Option<String>,
    ) -> Self {
        let mut state = Self {
            all_sessions: sessions,
            filtered_sessions: Vec::new(),
            current_cwd: current_cwd.into(),
            current_session_path,
            scope: SessionScope::Current,
            sort_mode: SessionSortMode::Threaded,
            name_filter: SessionNameFilter::All,
            selected: 0,
            max_visible: 10,
            show_path: false,
            confirming_delete: None,
            // The upstream startup picker renders a visible search prompt;
            // keeping it here also makes the real modal distinguishable from
            // the composer while it is focused.
            search: Input::new("  Search: "),
        };
        state.refresh();
        state
    }

    fn records_in_scope(&self) -> Vec<SessionPickerRecord> {
        if self.scope == SessionScope::All {
            return self.all_sessions.clone();
        }
        let current = canonical_session_path(&self.current_cwd);
        self.all_sessions
            .iter()
            .filter(|session| canonical_session_path(&session.cwd) == current)
            .cloned()
            .collect()
    }

    fn refresh(&mut self) {
        let records = self.records_in_scope();
        let query = self.search.get_value();
        let name_filtered = records
            .iter()
            .filter(|session| {
                self.name_filter == SessionNameFilter::All || has_session_name(session)
            })
            .cloned()
            .collect::<Vec<_>>();
        let rows = if self.sort_mode == SessionSortMode::Threaded && query.trim().is_empty() {
            flatten_session_tree(&build_session_tree(&name_filtered))
        } else {
            filter_and_sort_sessions(&records, query, self.sort_mode, self.name_filter)
                .into_iter()
                .map(|session| FlatSessionNode {
                    session,
                    depth: 0,
                    is_last: true,
                    ancestor_continues: Vec::new(),
                })
                .collect()
        };
        self.filtered_sessions = rows;
        self.selected = self
            .selected
            .min(self.filtered_sessions.len().saturating_sub(1));
    }

    pub fn set_sessions(&mut self, sessions: Vec<SessionPickerRecord>) {
        self.all_sessions = sessions;
        self.refresh();
    }

    pub fn selected_session(&self) -> Option<&SessionPickerRecord> {
        self.filtered_sessions
            .get(self.selected)
            .map(|row| &row.session)
    }

    pub fn filtered_sessions(&self) -> &[FlatSessionNode] {
        &self.filtered_sessions
    }

    pub fn search_query(&self) -> &str {
        self.search.get_value()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn scope(&self) -> SessionScope {
        self.scope
    }

    pub fn sort_mode(&self) -> SessionSortMode {
        self.sort_mode
    }

    pub fn name_filter(&self) -> SessionNameFilter {
        self.name_filter
    }

    pub fn show_path(&self) -> bool {
        self.show_path
    }

    pub fn confirming_delete(&self) -> Option<&str> {
        self.confirming_delete.as_deref()
    }

    fn selected_is_current(&self) -> bool {
        let Some(current) = self.current_session_path.as_deref() else {
            return false;
        };
        self.selected_session().is_some_and(|session| {
            canonical_session_path(&session.path) == canonical_session_path(current)
        })
    }

    fn start_delete(&mut self) -> SessionPickerAction {
        let Some(session) = self.selected_session() else {
            return SessionPickerAction::None;
        };
        if self.selected_is_current() {
            return SessionPickerAction::DeleteCurrentDenied(
                "Cannot delete the currently active session".to_string(),
            );
        }
        let path = session.path.clone();
        self.confirming_delete = Some(path.clone());
        SessionPickerAction::DeleteRequested(path)
    }

    fn cycle_sort(&mut self) -> SessionSortMode {
        self.sort_mode = match self.sort_mode {
            SessionSortMode::Threaded => SessionSortMode::Recent,
            SessionSortMode::Recent => SessionSortMode::Relevance,
            SessionSortMode::Relevance => SessionSortMode::Threaded,
        };
        self.refresh();
        self.sort_mode
    }

    pub fn handle(&mut self, key: &TuiKey) -> SessionPickerAction {
        if let Some(path) = self.confirming_delete.clone() {
            let keybindings = get_keybindings();
            if keybindings.matches(key, "tui.select.confirm") {
                self.confirming_delete = None;
                return SessionPickerAction::DeleteConfirmed(path);
            }
            if keybindings.matches(key, "tui.select.cancel") {
                self.confirming_delete = None;
                return SessionPickerAction::DeleteCancelled;
            }
            return SessionPickerAction::None;
        }

        let keybindings = get_keybindings();
        if keybindings.matches(key, "tui.input.tab") {
            self.scope = match self.scope {
                SessionScope::Current => SessionScope::All,
                SessionScope::All => SessionScope::Current,
            };
            self.selected = 0;
            self.refresh();
            return SessionPickerAction::ScopeChanged(self.scope);
        }
        if key.ctrl && !key.shift && !key.alt && key.base == "s" {
            return SessionPickerAction::SortChanged(self.cycle_sort());
        }
        if key.ctrl && !key.shift && !key.alt && key.base == "n" {
            self.name_filter = match self.name_filter {
                SessionNameFilter::All => SessionNameFilter::Named,
                SessionNameFilter::Named => SessionNameFilter::All,
            };
            self.selected = 0;
            self.refresh();
            return SessionPickerAction::NameFilterChanged(self.name_filter);
        }
        if key.ctrl && !key.shift && !key.alt && key.base == "p" {
            self.show_path = !self.show_path;
            return SessionPickerAction::PathVisibilityChanged(self.show_path);
        }
        if key.ctrl && !key.shift && !key.alt && key.base == "d" {
            return self.start_delete();
        }
        if key.ctrl && !key.shift && !key.alt && key.base == "r" {
            return self
                .selected_session()
                .map(|session| SessionPickerAction::BeginRename(session.path.clone()))
                .unwrap_or(SessionPickerAction::None);
        }
        if key.ctrl && key.base == "backspace" {
            if self.search.get_value().is_empty() {
                return self.start_delete();
            }
            self.search.handle_input(key);
            self.refresh();
            return SessionPickerAction::None;
        }
        if keybindings.matches(key, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
            return SessionPickerAction::None;
        }
        if keybindings.matches(key, "tui.select.down") {
            if !self.filtered_sessions.is_empty() {
                self.selected = (self.selected + 1).min(self.filtered_sessions.len() - 1);
            }
            return SessionPickerAction::None;
        }
        if keybindings.matches(key, "tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(self.max_visible.max(1));
            return SessionPickerAction::None;
        }
        if keybindings.matches(key, "tui.select.pageDown") {
            if !self.filtered_sessions.is_empty() {
                self.selected =
                    (self.selected + self.max_visible.max(1)).min(self.filtered_sessions.len() - 1);
            }
            return SessionPickerAction::None;
        }
        if keybindings.matches(key, "tui.select.confirm") {
            return self
                .selected_session()
                .map(|session| SessionPickerAction::Select {
                    id: session.id.clone(),
                    path: session.path.clone(),
                })
                .unwrap_or(SessionPickerAction::None);
        }
        if keybindings.matches(key, "tui.select.cancel") {
            return SessionPickerAction::Cancel;
        }

        self.search.handle_input(key);
        self.refresh();
        SessionPickerAction::None
    }

    fn tree_prefix(row: &FlatSessionNode) -> String {
        if row.depth == 0 {
            return String::new();
        }
        let mut prefix = row
            .ancestor_continues
            .iter()
            .map(|continues| if *continues { "│  " } else { "   " })
            .collect::<String>();
        prefix.push_str(if row.is_last { "└─ " } else { "├─ " });
        prefix
    }

    fn session_age(modified_at: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(modified_at);
        let elapsed = now.saturating_sub(modified_at);
        let minute = 60_000;
        let hour = 60 * minute;
        let day = 24 * hour;
        let week = 7 * day;
        let month = 30 * day;
        let year = 365 * day;
        if elapsed < minute {
            "now".to_string()
        } else if elapsed < hour {
            format!("{}m", elapsed / minute)
        } else if elapsed < day {
            format!("{}h", elapsed / hour)
        } else if elapsed < week {
            format!("{}d", elapsed / day)
        } else if elapsed < month {
            format!("{}w", elapsed / week)
        } else if elapsed < year {
            format!("{}mo", elapsed / month)
        } else {
            format!("{}y", elapsed / year)
        }
    }
}

impl Component for SessionPickerState {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.search.render(width);
        lines.push(String::new());
        if self.filtered_sessions.is_empty() {
            lines.push(
                if self.name_filter == SessionNameFilter::Named {
                    "  No named sessions found. Press Ctrl+N to show all."
                } else if self.scope == SessionScope::Current {
                    "  No sessions in current folder. Press Tab to view all."
                } else {
                    "  No sessions found"
                }
                .to_string(),
            );
            return lines
                .into_iter()
                .map(|line| truncate_to_width(&line, width, "…"))
                .collect();
        }

        let start = self.selected.saturating_sub(self.max_visible / 2).min(
            self.filtered_sessions
                .len()
                .saturating_sub(self.max_visible),
        );
        let end = (start + self.max_visible).min(self.filtered_sessions.len());
        for (index, row) in self.filtered_sessions[start..end].iter().enumerate() {
            let absolute = start + index;
            let selected = absolute == self.selected;
            let current = self.current_session_path.as_deref().is_some_and(|path| {
                canonical_session_path(path) == canonical_session_path(&row.session.path)
            });
            let deleting = self.confirming_delete.as_deref() == Some(row.session.path.as_str());
            let prefix = Self::tree_prefix(row);
            let display = row
                .session
                .display_text()
                .replace(|character: char| character.is_control(), " ")
                .trim()
                .to_string();
            let right = if self.show_path {
                format!(
                    "{} {} {}",
                    row.session.path,
                    row.session.message_count,
                    Self::session_age(row.session.modified_at)
                )
            } else {
                format!(
                    "{} {}",
                    row.session.message_count,
                    Self::session_age(row.session.modified_at)
                )
            };
            let left_prefix = if selected { "› " } else { "  " };
            let available = width
                .saturating_sub(visible_width(left_prefix))
                .saturating_sub(visible_width(&prefix))
                .saturating_sub(visible_width(&right))
                .saturating_sub(2)
                .max(10);
            let message = truncate_to_width(&display, available, "…");
            let message = if deleting {
                t::fg("error", message)
            } else if current {
                t::fg("accent", message)
            } else if row.session.has_name() {
                t::fg("warning", message)
            } else {
                message
            };
            let message = if selected { t::bold(message) } else { message };
            let spacing = " ".repeat(
                width
                    .saturating_sub(visible_width(left_prefix))
                    .saturating_sub(visible_width(&prefix))
                    .saturating_sub(visible_width(&message))
                    .saturating_sub(visible_width(&right))
                    .max(1),
            );
            let line = format!(
                "{}{}{}{}{}",
                left_prefix,
                t::fg("dim", prefix),
                message,
                spacing,
                t::fg("dim", right)
            );
            lines.push(truncate_to_width(&line, width, "…"));
        }
        if start > 0 || end < self.filtered_sessions.len() {
            lines.push(t::fg(
                "muted",
                format!("  ({}/{})", self.selected + 1, self.filtered_sessions.len()),
            ));
        }
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.focused = focused;
    }

    fn set_height(&mut self, height: usize) {
        if height > 0 {
            self.max_visible = height.saturating_sub(3).max(1);
        }
    }
}

/// The interactive equivalent of Pi's `[y/N]` cross-project session prompt.
///
/// This deliberately is not a selectable list: upstream's confirmation has a
/// default-negative answer and only accepts an explicit `y`/`yes` as consent
/// to create a new, parent-linked session in the current project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossProjectSessionAction {
    None,
    Confirm,
    Cancel,
}

pub struct CrossProjectSessionPrompt {
    source: SessionMetadata,
}

impl CrossProjectSessionPrompt {
    pub fn new(source: SessionMetadata) -> Self {
        Self { source }
    }

    pub fn source(&self) -> &SessionMetadata {
        &self.source
    }

    pub fn handle(&mut self, key: &TuiKey) -> CrossProjectSessionAction {
        if key.ctrl && key.base == "c" {
            return CrossProjectSessionAction::Cancel;
        }
        match key.base.as_str() {
            "y" | "Y" | "yes" | "YES" => CrossProjectSessionAction::Confirm,
            "n" | "N" | "no" | "NO" | "enter" | "return" | "esc" | "escape" => {
                CrossProjectSessionAction::Cancel
            }
            _ => CrossProjectSessionAction::None,
        }
    }
}

impl Component for CrossProjectSessionPrompt {
    fn render(&self, width: usize) -> Vec<String> {
        [
            format!("Session found in different project: {}", self.source.cwd),
            "Fork this session into current directory? [y/N]".to_string(),
        ]
        .into_iter()
        .map(|line| truncate_to_width(&line, width, "…"))
        .collect()
    }

    fn handle_input(&mut self, key: &TuiKey) {
        let _ = self.handle(key);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn meta(id: &str, modified: u64) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            created_at: 1,
            cwd: "/tmp/proj".to_string(),
            path: format!("/tmp/proj/sessions/2026-01-01T00-00-00_{id}.jsonl"),
            modified_at: modified,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }

    #[test]
    fn cross_project_prompt_requires_explicit_yes() {
        let mut prompt = CrossProjectSessionPrompt::new(SessionMetadata {
            cwd: "/other/project".into(),
            ..meta("other", 1)
        });
        assert_eq!(
            prompt.handle(&TuiKey::simple("enter")),
            CrossProjectSessionAction::Cancel
        );
        assert_eq!(
            prompt.handle(&TuiKey::simple("y")),
            CrossProjectSessionAction::Confirm
        );
    }

    #[test]
    fn cross_project_prompt_renders_source_and_question() {
        let prompt = CrossProjectSessionPrompt::new(SessionMetadata {
            cwd: "/other/project".into(),
            ..meta("other", 1)
        });
        let lines = prompt.render(80);
        assert!(lines[0].contains("/other/project"));
        assert!(lines[1].contains("[y/N]"));
    }

    #[test]
    fn session_cwd_comparison_normalizes_lexical_paths() {
        assert!(session_cwds_match(
            "/tmp/project/./nested/..",
            "/tmp/project"
        ));
        assert!(!session_cwds_match("/tmp/project-a", "/tmp/project-b"));
    }

    fn record(
        id: &str,
        modified_at: u64,
        name: Option<&str>,
        message: &str,
        parent_session_id: Option<&str>,
    ) -> SessionPickerRecord {
        SessionPickerRecord {
            id: id.to_string(),
            path: format!("/tmp/proj/sessions/{id}.jsonl"),
            cwd: "/tmp/proj".to_string(),
            name: name.map(str::to_string),
            created_at: modified_at,
            modified_at,
            message_count: 2,
            first_message: message.to_string(),
            all_messages_text: message.to_string(),
            parent_session_id: parent_session_id.map(str::to_string),
            parent_session_path: None,
        }
    }

    #[test]
    fn picker_sorts_newest_first_with_deterministic_ties() {
        let items = session_picker_items(vec![meta("z", 10), meta("new", 30), meta("a", 10)]);
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "a", "z"]
        );
    }

    #[test]
    fn picker_labels_use_file_names() {
        let items = session_picker_items(vec![meta("abc123", 10)]);
        assert_eq!(items[0].label, "2026-01-01T00-00-00_abc123.jsonl");
        let select = picker_select_items(&items);
        assert_eq!(select.len(), 1);
        assert_eq!(select[0].value, "abc123");
        assert_eq!(select[0].description.as_deref(), Some("/tmp/proj"));
    }

    #[test]
    fn startup_picker_search_matches_partial_id_and_exact_path() {
        let items = session_picker_items(vec![meta("abc123", 10)]);
        let path = items[0].metadata.path.clone();
        let mut selector = crate::interactive::selectors::ListSelector::new(
            picker_select_items_with_paths(&items),
            10,
        );

        selector.set_filter("abc12");
        assert_eq!(selector.filtered_count(), 1);
        assert_eq!(selector.selected_value().as_deref(), Some("abc123"));

        selector.set_filter(&path);
        assert_eq!(selector.filtered_count(), 1);
        assert_eq!(selector.selected_value().as_deref(), Some("abc123"));
    }

    #[test]
    fn search_parser_supports_tokens_phrases_regex_and_unclosed_quotes() {
        let parsed = parse_session_search_query("fix \"unicode input\" now");
        assert_eq!(parsed.mode, SessionSearchMode::Tokens);
        assert_eq!(
            parsed.tokens,
            vec![
                SessionSearchToken {
                    kind: SessionSearchTokenKind::Fuzzy,
                    value: "fix".into(),
                },
                SessionSearchToken {
                    kind: SessionSearchTokenKind::Phrase,
                    value: "unicode input".into(),
                },
                SessionSearchToken {
                    kind: SessionSearchTokenKind::Fuzzy,
                    value: "now".into(),
                },
            ]
        );

        let regex = parse_session_search_query(r"re:unicode\s+input");
        assert_eq!(regex.mode, SessionSearchMode::Regex);
        assert!(regex.error.is_none());
        assert_eq!(regex.regex.as_deref(), Some(r"unicode\s+input"));

        let unclosed = parse_session_search_query("fix \"unicode input");
        assert!(unclosed
            .tokens
            .iter()
            .all(|token| token.kind == SessionSearchTokenKind::Fuzzy));
        assert!(parse_session_search_query("re:[").error.is_some());
    }

    #[test]
    fn search_match_obeys_phrase_regex_and_fuzzy_scores() {
        let session = record(
            "s1",
            10,
            Some("Named session"),
            "Fix Unicode input lag",
            None,
        );
        let phrase = parse_session_search_query("\"unicode input\"");
        assert!(match_session(&session, &phrase).matches);
        let regex = parse_session_search_query("re:UNICODE");
        assert!(match_session(&session, &regex).matches);
        let fuzzy = parse_session_search_query("uil");
        assert!(match_session(&session, &fuzzy).matches);
        let missing = parse_session_search_query("missing");
        assert!(!match_session(&session, &missing).matches);
    }

    #[test]
    fn search_corpus_does_not_fallback_to_first_message() {
        let session = record("s1", 10, None, "first message only", None);
        let mut session = session;
        session.all_messages_text.clear();

        assert!(!match_session(&session, &parse_session_search_query("first")).matches);
        assert!(match_session(&session, &parse_session_search_query("s1")).matches);
    }

    #[test]
    fn filtering_respects_named_filter_recent_order_and_relevance() {
        let records = vec![
            record("old", 10, None, "other work", None),
            record("named", 30, Some("Unicode repair"), "other work", None),
            record("new", 40, Some("Other"), "Unicode repair", None),
        ];
        let named = filter_and_sort_sessions(
            &records,
            "unicode",
            SessionSortMode::Recent,
            SessionNameFilter::Named,
        );
        assert_eq!(
            named
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["named", "new"]
        );
        let relevance = filter_and_sort_sessions(
            &records,
            "\"unicode repair\"",
            SessionSortMode::Relevance,
            SessionNameFilter::All,
        );
        assert_eq!(relevance[0].id, "named");
    }

    #[test]
    fn tree_builds_id_and_path_parentage_and_survives_cycles() {
        let root = record("root", 10, None, "root", None);
        let child = record("child", 30, None, "child", Some("root"));
        let grandchild = record("grandchild", 20, None, "grandchild", Some("child"));
        let mut cycle = record("cycle", 5, None, "cycle", Some("root"));
        cycle.parent_session_id = Some("root".into());
        let roots = build_session_tree(&[root, child, grandchild, cycle]);
        let flat = flatten_session_tree(&roots);
        assert_eq!(
            flat.iter()
                .map(|row| (row.session.id.as_str(), row.depth))
                .collect::<Vec<_>>(),
            vec![("root", 0), ("child", 1), ("grandchild", 2), ("cycle", 1)]
        );
        assert_eq!(roots[0].latest_activity, 30);
    }

    #[test]
    fn session_state_handles_search_navigation_focus_and_actions() {
        let current = record("current", 30, None, "current", None);
        let other = record("other", 20, Some("Other"), "unicode", None);
        let mut picker = SessionPickerState::new(
            vec![current.clone(), other.clone()],
            "/tmp/proj",
            Some(current.path.clone()),
        );
        assert_eq!(picker.filtered_sessions().len(), 2);
        picker.handle(&TuiKey::simple("down"));
        assert_eq!(
            picker.selected_session().map(|session| session.id.as_str()),
            Some("other")
        );
        assert_eq!(
            picker.handle(&TuiKey::ctrl("d")),
            SessionPickerAction::DeleteRequested(other.path.clone())
        );
        assert_eq!(picker.confirming_delete(), Some(other.path.as_str()));
        assert_eq!(
            picker.handle(&TuiKey::simple("escape")),
            SessionPickerAction::DeleteCancelled
        );
        picker.handle(&TuiKey::simple("up"));
        assert!(matches!(
            picker.handle(&TuiKey::ctrl("d")),
            SessionPickerAction::DeleteCurrentDenied(_)
        ));
        for character in "unicode".chars() {
            picker.handle(&TuiKey::simple(character.to_string()));
        }
        assert_eq!(picker.search_query(), "unicode");
        assert_eq!(
            picker.handle(&TuiKey::simple("enter")),
            SessionPickerAction::Select {
                id: other.id,
                path: other.path,
            }
        );
        picker.set_focused(true);
        assert!(picker.render(100)[0].contains(pi_tui::CURSOR_MARKER));
    }

    #[test]
    fn session_state_toggles_scope_sort_name_and_path_without_wrapping() {
        let records = vec![
            record("one", 10, None, "one", None),
            record("two", 20, Some("two"), "two", None),
        ];
        let mut picker = SessionPickerState::new(records, "/other", None);
        assert!(picker.filtered_sessions().is_empty());
        assert_eq!(
            picker.handle(&TuiKey::simple("tab")),
            SessionPickerAction::ScopeChanged(SessionScope::All)
        );
        picker.handle(&TuiKey::simple("up"));
        assert_eq!(picker.selected_index(), 0);
        assert_eq!(
            picker.handle(&TuiKey::ctrl("s")),
            SessionPickerAction::SortChanged(SessionSortMode::Recent)
        );
        assert_eq!(
            picker.handle(&TuiKey::ctrl("n")),
            SessionPickerAction::NameFilterChanged(SessionNameFilter::Named)
        );
        assert_eq!(
            picker.handle(&TuiKey::ctrl("p")),
            SessionPickerAction::PathVisibilityChanged(true)
        );
    }
}
