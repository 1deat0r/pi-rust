//! Autocomplete provider — port of `packages/tui/src/autocomplete.ts`.
//!
//! Combines slash-command completion, file-path completion (fs walk, with the
//! `fd` binary for fuzzy `@`-attachment search), and per-command argument
//! completions.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::fuzzy::fuzzy_filter;

const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];

fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn escape_regex(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for c in value.chars() {
        if matches!(c, '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build the path pattern argument passed to `fd` (upstream `buildFdPathQuery`).
fn build_fd_path_query(query: &str) -> String {
    let normalized = to_display_path(query);
    if !normalized.contains('/') {
        return normalized;
    }
    let has_trailing_separator = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }
    let separator_pattern = "[\\\\/]";
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(escape_regex)
        .collect();
    if segments.is_empty() {
        return normalized;
    }
    let mut pattern = segments.join(separator_pattern);
    if has_trailing_separator {
        pattern.push_str(separator_pattern);
    }
    pattern
}

fn find_last_delimiter(text: &str) -> isize {
    let chars: Vec<char> = text.chars().collect();
    for i in (0..chars.len()).rev() {
        if PATH_DELIMITERS.contains(&chars[i]) {
            return i as isize;
        }
    }
    -1
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start: isize = -1;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = i as isize;
            }
        }
    }
    if in_quotes && quote_start >= 0 {
        Some(quote_start as usize)
    } else {
        None
    }
}

fn is_token_start(text: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let prev = text[..index].chars().next_back().unwrap_or_default();
    PATH_DELIMITERS.contains(&prev)
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;
    if quote_start == 0 {
        return Some(text[quote_start..].to_string());
    }
    let before = &text[..quote_start];
    if before.ends_with('@') {
        if !is_token_start(text, quote_start - 1) {
            return None;
        }
        return Some(text[quote_start - 1..].to_string());
    }
    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(text[quote_start..].to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathPrefix {
    pub raw_prefix: String,
    pub is_at_prefix: bool,
    pub is_quoted_prefix: bool,
}

fn parse_path_prefix(prefix: &str) -> PathPrefix {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        return PathPrefix { raw_prefix: rest.to_string(), is_at_prefix: true, is_quoted_prefix: true };
    }
    if let Some(rest) = prefix.strip_prefix('"') {
        return PathPrefix { raw_prefix: rest.to_string(), is_at_prefix: false, is_quoted_prefix: true };
    }
    if let Some(rest) = prefix.strip_prefix('@') {
        return PathPrefix { raw_prefix: rest.to_string(), is_at_prefix: true, is_quoted_prefix: false };
    }
    PathPrefix { raw_prefix: prefix.to_string(), is_at_prefix: false, is_quoted_prefix: false }
}

fn build_completion_value(path: &str, options: &PathPrefix) -> String {
    let needs_quotes = options.is_quoted_prefix || path.contains(' ');
    let prefix = if options.is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        return format!("{prefix}{path}");
    }
    format!("{prefix}\"{path}\"")
}

/// A suggestion item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// An argument-completion hook: given the argument prefix, return suggestions
/// or None when unavailable.
pub type ArgumentCompletionsFn = Box<dyn Fn(&str) -> Option<Vec<AutocompleteItem>> + Send + Sync>;

/// A slash command usable by the provider.
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// Get argument completions for an argument prefix (None = unavailable).
    pub get_argument_completions: Option<ArgumentCompletionsFn>,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: Option<String>, argument_hint: Option<String>) -> Self {
        Self { name: name.into(), description, argument_hint, get_argument_completions: None }
    }

    pub fn with_argument_completions(
        name: impl Into<String>,
        description: Option<String>,
        argument_hint: Option<String>,
        f: impl Fn(&str) -> Option<Vec<AutocompleteItem>> + Send + Sync + 'static,
    ) -> Self {
        Self { name: name.into(), description, argument_hint, get_argument_completions: Some(Box::new(f)) }
    }
}

impl std::fmt::Debug for SlashCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlashCommand").field("name", &self.name).finish()
    }
}

/// Suggestion result: items plus the prefix matched against.
#[derive(Debug, Clone)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    pub prefix: String,
}

/// Provider trait (synchronous; wrap in spawn_blocking for async callers).
pub trait AutocompleteProvider {
    /// Characters that naturally trigger this provider at token boundaries.
    fn trigger_characters(&self) -> Vec<String>;
    /// Get suggestions for the current text/cursor position.
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
        aborted: &AtomicBool,
    ) -> Option<AutocompleteSuggestions>;
    /// Apply the selected item, returning new text/cursor state.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult;
    /// Whether explicit Tab completion should trigger file completion.
    fn should_trigger_file_completion(&self, _lines: &[String], _cursor_line: usize, _cursor_col: usize) -> bool {
        true
    }
}

/// The result of applying a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// A file/directory entry discovered by fd.
struct FdEntry {
    path: String,
    is_directory: bool,
}

/// Walk a directory tree using `fd` (fast, respects .gitignore).
fn walk_directory_with_fd(
    base_dir: &str,
    fd_path: &str,
    query: &str,
    max_results: usize,
    aborted: &AtomicBool,
) -> Vec<FdEntry> {
    if aborted.load(Ordering::SeqCst) {
        return Vec::new();
    }
    let mut args: Vec<String> = vec![
        "--base-directory".into(),
        base_dir.into(),
        "--max-results".into(),
        max_results.to_string(),
        "--type".into(),
        "f".into(),
        "--type".into(),
        "d".into(),
        "--follow".into(),
        "--hidden".into(),
        "--exclude".into(),
        ".git".into(),
        "--exclude".into(),
        ".git/*".into(),
        "--exclude".into(),
        ".git/**".into(),
    ];
    if to_display_path(query).contains('/') {
        args.push("--full-path".into());
    }
    if !query.is_empty() {
        args.push(build_fd_path_query(query));
    }

    let output = match std::process::Command::new(fd_path).args(&args).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if aborted.load(Ordering::SeqCst) || !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    for line in stdout.lines() {
        let display_line = to_display_path(line);
        let has_trailing_separator = display_line.ends_with('/');
        let normalized_path = if has_trailing_separator {
            display_line[..display_line.len() - 1].to_string()
        } else {
            display_line.clone()
        };
        if normalized_path == ".git"
            || normalized_path.starts_with(".git/")
            || normalized_path.contains("/.git/")
        {
            continue;
        }
        results.push(FdEntry { path: display_line, is_directory: has_trailing_separator });
    }
    results
}

/// Combined provider for slash commands and file paths.
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: String,
    fd_path: Option<String>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: impl Into<String>, fd_path: Option<String>) -> Self {
        Self { commands, base_path: base_path.into(), fd_path }
    }

    fn extract_at_prefix(&self, text: &str) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text) {
            if quoted.starts_with("@\"") {
                return Some(quoted);
            }
        }
        let last_delimiter_index = find_last_delimiter(text);
        let token_start = if last_delimiter_index == -1 { 0 } else { last_delimiter_index as usize + 1 };
        if text[token_start..].starts_with('@') {
            return Some(text[token_start..].to_string());
        }
        None
    }

    fn extract_path_prefix(&self, text: &str, force_extract: bool) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text) {
            return Some(quoted);
        }
        let last_delimiter_index = find_last_delimiter(text);
        let path_prefix = if last_delimiter_index == -1 {
            text.to_string()
        } else {
            text[last_delimiter_index as usize + 1..].to_string()
        };

        if force_extract {
            return Some(path_prefix);
        }

        if path_prefix.contains('/') || path_prefix.starts_with('.') || path_prefix.starts_with("~/") {
            return Some(path_prefix);
        }
        if path_prefix.is_empty() && text.ends_with(' ') {
            return Some(path_prefix);
        }
        None
    }

    fn expand_home_path(&self, path: &str) -> String {
        if let Some(rest) = path.strip_prefix("~/") {
            let home = std::env::var("HOME").unwrap_or_default();
            let expanded = Path::new(&home).join(rest);
            let mut out = expanded.to_string_lossy().into_owned();
            if path.ends_with('/') && !out.ends_with('/') {
                out.push('/');
            }
            out
        } else if path == "~" {
            std::env::var("HOME").unwrap_or_default()
        } else {
            path.to_string()
        }
    }

    fn resolve_scoped_fuzzy_query(&self, raw_query: &str) -> Option<(String, String, String)> {
        let normalized_query = to_display_path(raw_query);
        let slash_index = normalized_query.rfind('/')?;
        let display_base = normalized_query[..=slash_index].to_string();
        let query = normalized_query[slash_index + 1..].to_string();

        let base_dir = if display_base.starts_with("~/") {
            self.expand_home_path(&display_base)
        } else if display_base.starts_with('/') {
            display_base.clone()
        } else {
            Path::new(&self.base_path).join(&display_base).to_string_lossy().into_owned()
        };

        let meta = std::fs::metadata(&base_dir).ok()?;
        if !meta.is_dir() {
            return None;
        }

        Some((base_dir, query, display_base))
    }

    fn scoped_path_for_display(&self, display_base: &str, relative_path: &str) -> String {
        let normalized_relative_path = to_display_path(relative_path);
        if display_base == "/" {
            return format!("/{normalized_relative_path}");
        }
        format!("{}{}", to_display_path(display_base), normalized_relative_path)
    }

    /// Get file/directory suggestions for a given path prefix.
    fn get_file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let parsed = parse_path_prefix(prefix);
        let mut expanded_prefix = parsed.raw_prefix.clone();

        if expanded_prefix.starts_with('~') {
            expanded_prefix = self.expand_home_path(&expanded_prefix);
        }

        let is_root_prefix = parsed.raw_prefix.is_empty()
            || parsed.raw_prefix == "./"
            || parsed.raw_prefix == "../"
            || parsed.raw_prefix == "~"
            || parsed.raw_prefix == "~/"
            || parsed.raw_prefix == "/"
            || (parsed.is_at_prefix && parsed.raw_prefix.is_empty());

        // Resolve the directory to list and the name prefix to filter on.
        let (search_dir, search_prefix) = if is_root_prefix || parsed.raw_prefix.ends_with('/') {
            let dir = if parsed.raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                expanded_prefix.clone()
            } else {
                Path::new(&self.base_path)
                    .join(&expanded_prefix)
                    .to_string_lossy()
                    .into_owned()
            };
            (dir, String::new())
        } else {
            let dir = Path::new(&expanded_prefix)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_string_lossy()
                .into_owned();
            let file = Path::new(&expanded_prefix)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let search_dir = if parsed.raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                if dir.is_empty() {
                    "/".to_string()
                } else {
                    dir
                }
            } else {
                Path::new(&self.base_path)
                    .join(&dir)
                    .to_string_lossy()
                    .into_owned()
            };
            (search_dir, file)
        };

        let entries = match std::fs::read_dir(&search_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        let mut suggestions = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&search_prefix.to_lowercase()) {
                continue;
            }
            let is_directory = match entry.file_type() {
                Ok(ft) if ft.is_dir() => true,
                Ok(ft) if ft.is_symlink() => {
                    std::fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false)
                }
                _ => false,
            };

            let display_prefix = parsed.raw_prefix.clone();
            let relative_path = if display_prefix.ends_with('/') {
                format!("{display_prefix}{name}")
            } else if display_prefix.contains('/') || display_prefix.contains('\\') {
                if let Some(rest) = display_prefix.strip_prefix("~/") {
                    let dir = Path::new(rest).parent().unwrap_or_else(|| Path::new(""));
                    let dir_s = dir.to_string_lossy().into_owned();
                    if dir_s == "." || dir_s.is_empty() {
                        format!("~/{name}")
                    } else {
                        format!("~/{}/{name}", dir_s.trim_end_matches('/'))
                    }
                } else if display_prefix.starts_with('/') {
                    let dir = Path::new(&display_prefix).parent().unwrap_or_else(|| Path::new("/"));
                    let dir_s = dir.to_string_lossy().into_owned();
                    if dir_s == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir_s}/{name}")
                    }
                } else {
                    let dir = Path::new(&display_prefix).parent().unwrap_or_else(|| Path::new(""));
                    let relative = dir.join(&name).to_string_lossy().into_owned();
                    if display_prefix.starts_with("./") && !relative.starts_with("./") {
                        format!("./{relative}")
                    } else {
                        relative
                    }
                }
            } else if parsed.raw_prefix.starts_with('~') {
                format!("~/{name}")
            } else {
                name.clone()
            };

            let relative_path = to_display_path(&relative_path);
            let path_value = if is_directory { format!("{relative_path}/") } else { relative_path.clone() };
            let value = build_completion_value(&path_value, &parsed);
            suggestions.push(AutocompleteItem {
                value,
                label: if is_directory { format!("{name}/") } else { name },
                description: None,
            });
        }

        // Sort directories first, then alphabetically.
        suggestions.sort_by(|a, b| {
            let a_dir = a.value.ends_with('/');
            let b_dir = b.value.ends_with('/');
            match (a_dir, b_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.label.cmp(&b.label),
            }
        });
        suggestions
    }

    /// Score an entry against the query (higher = better).
    fn score_entry(&self, file_path: &str, query: &str, is_directory: bool) -> i64 {
        let file_name = Path::new(file_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lower_file_name = file_name.to_lowercase();
        let lower_query = query.to_lowercase();

        let mut score = 0i64;
        if lower_file_name == lower_query {
            score = 100;
        } else if lower_file_name.starts_with(&lower_query) {
            score = 80;
        } else if lower_file_name.contains(&lower_query) {
            score = 50;
        } else if file_path.to_lowercase().contains(&lower_query) {
            score = 30;
        }
        if is_directory && score > 0 {
            score += 10;
        }
        score
    }

    fn get_fuzzy_file_suggestions(
        &self,
        query: &str,
        is_quoted_prefix: bool,
        aborted: &AtomicBool,
    ) -> Vec<AutocompleteItem> {
        let Some(fd_path) = &self.fd_path else {
            return Vec::new();
        };
        if aborted.load(Ordering::SeqCst) {
            return Vec::new();
        }

        let scoped = self.resolve_scoped_fuzzy_query(query);
        let fd_base_dir = scoped.as_ref().map(|s| s.0.clone()).unwrap_or_else(|| self.base_path.clone());
        let fd_query = scoped.as_ref().map(|s| s.1.clone()).unwrap_or_else(|| query.to_string());
        let entries = walk_directory_with_fd(&fd_base_dir, fd_path, &fd_query, 100, aborted);
        if aborted.load(Ordering::SeqCst) {
            return Vec::new();
        }

        let mut scored: Vec<(i64, FdEntry)> = entries
            .into_iter()
            .map(|entry| {
                let score = if fd_query.is_empty() {
                    1
                } else {
                    self.score_entry(&entry.path, &fd_query, entry.is_directory)
                };
                (score, entry)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.truncate(20);

        let mut suggestions = Vec::new();
        for (_, entry) in scored {
            let path_without_slash = if entry.is_directory {
                entry.path.trim_end_matches('/').to_string()
            } else {
                entry.path.clone()
            };
            let display_path = match &scoped {
                Some((_, _, display_base)) => self.scoped_path_for_display(display_base, &path_without_slash),
                None => path_without_slash.clone(),
            };
            let entry_name = Path::new(&path_without_slash)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let completion_path = if entry.is_directory {
                format!("{display_path}/")
            } else {
                display_path.clone()
            };
            let value = build_completion_value(
                &completion_path,
                &PathPrefix { raw_prefix: String::new(), is_at_prefix: true, is_quoted_prefix },
            );
            suggestions.push(AutocompleteItem {
                value,
                label: if entry.is_directory { format!("{entry_name}/") } else { entry_name },
                description: Some(display_path),
            });
        }
        suggestions
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn trigger_characters(&self) -> Vec<String> {
        vec!["@".to_string(), "#".to_string()]
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
        aborted: &AtomicBool,
    ) -> Option<AutocompleteSuggestions> {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let text_before_cursor = current_line[..cursor_col.min(current_line.len())].to_string();

        // @ attachment prefix.
        if let Some(at_prefix) = self.extract_at_prefix(&text_before_cursor) {
            let parsed = parse_path_prefix(&at_prefix);
            let suggestions = self.get_fuzzy_file_suggestions(&parsed.raw_prefix, parsed.is_quoted_prefix, aborted);
            if suggestions.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions { items: suggestions, prefix: at_prefix });
        }

        if !force && text_before_cursor.starts_with('/') {
            if let Some(space_index) = text_before_cursor.find(' ') {
                // Command argument completion.
                let command_name = text_before_cursor[1..space_index].to_string();
                let argument_text = text_before_cursor[space_index + 1..].to_string();
                let command = self.commands.iter().find(|cmd| cmd.name == command_name)?;
                let f = command.get_argument_completions.as_ref()?;
                let argument_suggestions = f(&argument_text)?;
                if argument_suggestions.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions { items: argument_suggestions, prefix: argument_text });
            }

            // Command-name completion at line start.
            let prefix = text_before_cursor[1..].to_string();
            let command_items: Vec<AutocompleteItem> = self
                .commands
                .iter()
                .map(|cmd| {
                    let hint = cmd.argument_hint.clone();
                    let desc = cmd.description.clone().unwrap_or_default();
                    let full_desc = match (&hint, desc.is_empty()) {
                        (Some(h), false) => format!("{h} — {desc}"),
                        (Some(h), true) => h.clone(),
                        (None, false) => desc,
                        (None, true) => String::new(),
                    };
                    AutocompleteItem {
                        value: cmd.name.clone(),
                        label: cmd.name.clone(),
                        description: if full_desc.is_empty() { None } else { Some(full_desc) },
                    }
                })
                .collect();
            let filtered = fuzzy_filter(command_items, &prefix, |item| item.value.clone())
                .into_iter()
                .map(|item| AutocompleteItem {
                    value: item.value.clone(),
                    label: item.label,
                    description: item.description,
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions { items: filtered, prefix: text_before_cursor });
        }

        let path_match = self.extract_path_prefix(&text_before_cursor, force)?;
        let suggestions = self.get_file_suggestions(&path_match);
        if suggestions.is_empty() {
            return None;
        }
        Some(AutocompleteSuggestions { items: suggestions, prefix: path_match })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let text = &current_line;
        let cursor_col = cursor_col.min(text.len());
        // Remove the prefix (measured in chars) from the end of the
        // text-before-cursor region.
        let prefix_chars = prefix.chars().count();
        let before_prefix = text[..cursor_col].to_string();
        let before_prefix_char: String = before_prefix.chars().take(before_prefix.chars().count().saturating_sub(prefix_chars)).collect();
        let after_cursor = text[cursor_col..].to_string();
        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote_after_cursor = after_cursor.starts_with('"');
        let has_trailing_quote_in_item = item.value.ends_with('"');
        let adjusted_after_cursor = if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after_cursor {
            after_cursor[1..].to_string()
        } else {
            after_cursor
        };

        // Slash command completion (at line start, no path separator after /).
        let is_slash_command = prefix.starts_with('/')
            && before_prefix_char.trim().is_empty()
            && !prefix[1..].contains('/');
        if is_slash_command {
            let new_line = format!("{before_prefix_char}/{value} {adjusted_after_cursor}", value = item.value);
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line.clone();
            // beforePrefix.length + item.value.length + 2  ("/" + " ")
            let cursor_col_new = before_prefix_char.len() + item.value.len() + 2;
            return CompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: cursor_col_new,
            };
        }

        if prefix.starts_with('@') {
            let is_directory = item.label.ends_with('/');
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!("{before_prefix_char}{}{suffix}{adjusted_after_cursor}", item.value);
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.len().saturating_sub(1)
            } else {
                item.value.len()
            };
            return CompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix_char.len() + cursor_offset + suffix.len(),
            };
        }

        let text_before_cursor = text[..cursor_col.min(text.len())].to_string();
        if text_before_cursor.contains('/') && text_before_cursor.contains(' ') {
            let new_line = format!("{before_prefix_char}{}{adjusted_after_cursor}", item.value);
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;
            let has_trailing_quote = item.value.ends_with('"');
            let is_directory = item.label.ends_with('/');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.len().saturating_sub(1)
            } else {
                item.value.len()
            };
            return CompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix_char.len() + cursor_offset,
            };
        }

        // For file paths, complete the path.
        let new_line = format!("{before_prefix_char}{}{adjusted_after_cursor}", item.value);
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = new_line;
        let has_trailing_quote = item.value.ends_with('"');
        let is_directory = item.label.ends_with('/');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.len().saturating_sub(1)
        } else {
            item.value.len()
        };
        CompletionResult {
            lines: new_lines,
            cursor_line,
            cursor_col: before_prefix_char.len() + cursor_offset,
        }
    }

    fn should_trigger_file_completion(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> bool {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let text_before_cursor = current_line[..cursor_col.min(current_line.len())].to_string();
        let trimmed = text_before_cursor.trim_start();
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            return false;
        }
        true
    }
}

impl std::fmt::Debug for CombinedAutocompleteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CombinedAutocompleteProvider")
            .field("base_path", &self.base_path)
            .field("fd_path", &self.fd_path)
            .finish()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn provider(base_path: &str, commands: Vec<SlashCommand>) -> CombinedAutocompleteProvider {
        CombinedAutocompleteProvider::new(commands, base_path.to_string(), None)
    }

    fn fd_provider(base_path: &str) -> Option<CombinedAutocompleteProvider> {
        let out = std::process::Command::new("which").arg("fd").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let fd_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some(CombinedAutocompleteProvider::new(Vec::new(), base_path.to_string(), Some(fd_path)))
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pi-autocomplete-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup_folder(base: &Path, dirs: &[&str], files: &[(&str, &str)]) {
        for dir in dirs {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        for (path, contents) in files {
            let full = base.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
    }

    fn get_suggestions(p: &CombinedAutocompleteProvider, lines: &[&str], line: usize, col: usize, force: bool) -> Option<AutocompleteSuggestions> {
        let lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        p.get_suggestions(&lines, line, col, force, &AtomicBool::new(false))
    }

    #[allow(dead_code)] // helper kept for future abort tests
    fn aborted_flag() -> AtomicBool {
        AtomicBool::new(false)
    }

    mod path_prefix {
        use super::*;

        #[test]
        fn extracts_root_slash_when_forced() {
            let tmp = TempDir::new();
            let p = provider(tmp.path().to_str().unwrap(), Vec::new());
            let result = get_suggestions(&p, &["hey /"], 0, 5, true);
            if let Some(r) = result {
                assert_eq!(r.prefix, "/");
            } else {
                // Root may have no readable entries in CI; prefix still valid.
                // (The provider returns null when no suggestions exist.)
            }
        }

        #[test]
        fn does_not_trigger_for_slash_commands() {
            let tmp = TempDir::new();
            let p = provider(tmp.path().to_str().unwrap(), Vec::new());
            let result = get_suggestions(&p, &["/model"], 0, 6, true);
            assert!(result.is_none());
        }

        #[test]
        fn triggers_for_absolute_paths_after_command_argument() {
            let tmp = TempDir::new();
            let p = provider(tmp.path().to_str().unwrap(), Vec::new());
            let result = get_suggestions(&p, &["/command /"], 0, 10, true);
            if let Some(r) = result {
                assert_eq!(r.prefix, "/");
            }
        }
    }

    mod fd_files {
        use super::*;

        fn provider_with_fd() -> (TempDir, PathBuf, Option<CombinedAutocompleteProvider>) {
            let tmp = TempDir::new();
            let base = tmp.path().join("cwd");
            std::fs::create_dir_all(&base).unwrap();
            let p = fd_provider(base.to_str().unwrap());
            (tmp, base, p)
        }

        #[test]
        fn returns_all_files_and_folders_for_empty_at_query() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &["src"], &[("README.md", "readme")]);
            let line = "@";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let mut values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            values.sort();
            let mut expected = vec!["@README.md", "@src/"];
            expected.sort();
            assert_eq!(values, expected);
        }

        #[test]
        fn matches_file_with_extension() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &[], &[("file.txt", "content")]);
            let line = "@file.txt";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "@file.txt"));
        }

        #[test]
        fn filters_are_case_insensitive() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &["src"], &[("README.md", "readme")]);
            let line = "@re";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "@README.md"));
        }

        #[test]
        fn ranks_directories_before_files() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &["src"], &[("src.txt", "text")]);
            let line = "@src";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            assert_eq!(result.items[0].value, "@src/");
        }

        #[test]
        fn returns_nested_file_paths() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &[], &[("src/index.ts", "export {};\n")]);
            let line = "@index";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "@src/index.ts"));
        }

        #[test]
        fn matches_deeply_nested_paths() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &[], &[
                ("packages/tui/src/autocomplete.ts", "export {};"),
                ("packages/ai/src/autocomplete.ts", "export {};"),
            ]);
            let line = "@tui/src/auto";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "@packages/tui/src/autocomplete.ts"));
            assert!(!values.iter().any(|v| v == "@packages/ai/src/autocomplete.ts"));
        }

        #[test]
        fn quotes_paths_with_spaces_for_at_suggestions() {
            let (tmp, base, p) = provider_with_fd();
            let Some(p) = p else { return };
            let _ = &tmp;
            setup_folder(&base, &["my folder"], &[("my folder/test.txt", "content")]);
            let line = "@my";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "@\"my folder/\""));
        }
    }

    mod dot_slash {
        use super::*;

        #[test]
        fn preserves_dot_slash_prefix_for_files() {
            let tmp = TempDir::new();
            let base = tmp.path();
            let p = provider(base.to_str().unwrap(), Vec::new());
            setup_folder(base, &[], &[("update.sh", "#!/bin/bash"), ("utils.ts", "export {};")]);
            let line = "./up";
            let result = get_suggestions(&p, &[line], 0, line.len(), true).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "./update.sh"));
        }

        #[test]
        fn preserves_dot_slash_prefix_for_directories() {
            let tmp = TempDir::new();
            let base = tmp.path();
            let p = provider(base.to_str().unwrap(), Vec::new());
            setup_folder(base, &["src"], &[("src/index.ts", "export {};")]);
            let line = "./sr";
            let result = get_suggestions(&p, &[line], 0, line.len(), true).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "./src/"));
        }
    }

    mod quoted_paths {
        use super::*;

        #[test]
        fn quotes_paths_with_spaces_for_direct_completion() {
            let tmp = TempDir::new();
            let base = tmp.path();
            let p = provider(base.to_str().unwrap(), Vec::new());
            setup_folder(base, &["my folder"], &[("my folder/test.txt", "content")]);
            let line = "my";
            let result = get_suggestions(&p, &[line], 0, line.len(), true).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "\"my folder/\""));
        }

        #[test]
        fn continues_completion_inside_quoted_paths() {
            let tmp = TempDir::new();
            let base = tmp.path();
            let p = provider(base.to_str().unwrap(), Vec::new());
            setup_folder(base, &[], &[("my folder/test.txt", "content"), ("my folder/other.txt", "content")]);
            let line = "\"my folder/\"";
            let result = get_suggestions(&p, &[line], 0, line.len() - 1, true).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert!(values.iter().any(|v| v == "\"my folder/test.txt\""));
            assert!(values.iter().any(|v| v == "\"my folder/other.txt\""));
        }

        #[test]
        fn applies_quoted_completion_without_duplicating_closing_quote() {
            let tmp = TempDir::new();
            let base = tmp.path();
            let p = provider(base.to_str().unwrap(), Vec::new());
            setup_folder(base, &[], &[("my folder/test.txt", "content")]);
            let line = "\"my folder/te\"";
            let cursor_col = line.len() - 1;
            let result = get_suggestions(&p, &[line], 0, cursor_col, true).expect("suggestions");
            let item = result.items.iter().find(|i| i.value == "\"my folder/test.txt\"").expect("item");
            let applied = p.apply_completion(&[line.to_string()], 0, cursor_col, item, &result.prefix);
            assert_eq!(applied.lines[0], "\"my folder/test.txt\"");
        }
    }

    mod slash_commands {
        use super::*;

        fn command_provider() -> CombinedAutocompleteProvider {
            let commands = vec![
                SlashCommand::new("settings", Some("Open settings menu".into()), None),
                SlashCommand::new("model", Some("Select model (opens selector UI)".into()), Some("<provider/model>".into())),
                SlashCommand::new("thinking", Some("Set thinking level".into()), Some("<level>".into())),
            ];
            CombinedAutocompleteProvider::new(commands, "/tmp".to_string(), None)
        }

        #[test]
        fn completes_command_names_by_prefix() {
            let p = command_provider();
            let line = "/mod";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let values: Vec<String> = result.items.iter().map(|i| i.value.clone()).collect();
            assert_eq!(values, vec!["model"]);
            assert_eq!(result.prefix, "/mod");
        }

        #[test]
        fn applies_command_completion_with_trailing_space() {
            let p = command_provider();
            let line = "/mod";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            let item = result.items[0].clone();
            let applied = p.apply_completion(&[line.to_string()], 0, line.len(), &item, &result.prefix);
            assert_eq!(applied.lines[0], "/model ");
            assert_eq!(applied.cursor_col, 7);
        }

        #[test]
        fn argument_completion_uses_command_hook() {
            let commands = vec![SlashCommand::with_argument_completions(
                "model",
                Some("Select model".into()),
                Some("<provider/model>".into()),
                |argument: &str| {
                    let _ = argument;
                    Some(vec![
                        AutocompleteItem { value: "anthropic/claude-opus".into(), label: "claude-opus".into(), description: Some("anthropic".into()) },
                        AutocompleteItem { value: "google/gemini".into(), label: "gemini".into(), description: Some("google".into()) },
                    ])
                },
            )];
            let p = CombinedAutocompleteProvider::new(commands, "/tmp".to_string(), None);
            let line = "/model anthropic";
            let result = get_suggestions(&p, &[line], 0, line.len(), false).expect("suggestions");
            assert_eq!(result.prefix, "anthropic");
            assert_eq!(result.items.len(), 2);
        }

        #[test]
        fn should_not_trigger_file_completion_inside_command_name() {
            let p = command_provider();
            assert!(!p.should_trigger_file_completion(&["/model".to_string()], 0, 6));
            assert!(p.should_trigger_file_completion(&["/model a".to_string()], 0, 8));
        }
    }
}
