//! Global keybinding registry — port of `packages/tui/src/keybindings.ts`.
//!
//! Downstream packages can override bindings through a `KeybindingsManager`
//! constructed with the default definitions plus user overrides.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use crate::keys::{match_key, TuiKey};

/// A named keybinding ("tui.editor.cursorUp", ...).
pub type Keybinding = &'static str;

/// A single key id ("enter", "ctrl+c", "shift+enter", ...).
pub type KeyId = &'static str;

/// Default keybinding definitions, mirroring upstream `TUI_KEYBINDINGS`.
#[derive(Clone, Debug)]
pub struct KeybindingDefinition {
    pub id: Keybinding,
    pub default_keys: &'static [KeyId],
    pub description: Option<&'static str>,
}

macro_rules! kb {
    ($id:tt, [$($k:expr),*]) => {
        KeybindingDefinition {
            id: $id,
            default_keys: &[$($k),*],
            description: keybinding_description!($id),
        }
    };
}

/// Descriptions are part of the public upstream definition and are consumed
/// by settings/help surfaces even when a binding has no default key.
macro_rules! keybinding_description {
    ("tui.editor.cursorUp") => {
        Some("Move cursor up")
    };
    ("tui.editor.cursorDown") => {
        Some("Move cursor down")
    };
    ("tui.editor.historyPrevious") => {
        Some("Select previous prompt history entry")
    };
    ("tui.editor.historyNext") => {
        Some("Select next prompt history entry")
    };
    ("tui.editor.cursorLeft") => {
        Some("Move cursor left")
    };
    ("tui.editor.cursorRight") => {
        Some("Move cursor right")
    };
    ("tui.editor.cursorWordLeft") => {
        Some("Move cursor word left")
    };
    ("tui.editor.cursorWordRight") => {
        Some("Move cursor word right")
    };
    ("tui.editor.cursorLineStart") => {
        Some("Move to line start")
    };
    ("tui.editor.cursorLineEnd") => {
        Some("Move to line end")
    };
    ("tui.editor.jumpForward") => {
        Some("Jump forward to character")
    };
    ("tui.editor.jumpBackward") => {
        Some("Jump backward to character")
    };
    ("tui.editor.pageUp") => {
        Some("Page up")
    };
    ("tui.editor.pageDown") => {
        Some("Page down")
    };
    ("tui.editor.deleteCharBackward") => {
        Some("Delete character backward")
    };
    ("tui.editor.deleteCharForward") => {
        Some("Delete character forward")
    };
    ("tui.editor.deleteWordBackward") => {
        Some("Delete word backward")
    };
    ("tui.editor.deleteWordForward") => {
        Some("Delete word forward")
    };
    ("tui.editor.deleteToLineStart") => {
        Some("Delete to line start")
    };
    ("tui.editor.deleteToLineEnd") => {
        Some("Delete to line end")
    };
    ("tui.editor.yank") => {
        Some("Yank")
    };
    ("tui.editor.yankPop") => {
        Some("Yank pop")
    };
    ("tui.editor.undo") => {
        Some("Undo")
    };
    ("tui.input.newLine") => {
        Some("Insert newline")
    };
    ("tui.input.submit") => {
        Some("Submit input")
    };
    ("tui.input.tab") => {
        Some("Tab / autocomplete")
    };
    ("tui.input.copy") => {
        Some("Copy selection")
    };
    ("tui.select.up") => {
        Some("Move selection up")
    };
    ("tui.select.down") => {
        Some("Move selection down")
    };
    ("tui.select.pageUp") => {
        Some("Selection page up")
    };
    ("tui.select.pageDown") => {
        Some("Selection page down")
    };
    ("tui.select.confirm") => {
        Some("Confirm selection")
    };
    ("tui.select.cancel") => {
        Some("Cancel selection")
    };
    ("tui.altScreen.pageUp") => {
        Some("Scroll viewport up one page")
    };
    ("tui.altScreen.pageDown") => {
        Some("Scroll viewport down one page")
    };
    ("tui.altScreen.halfPageUp") => {
        Some("Scroll viewport up half a page")
    };
    ("tui.altScreen.halfPageDown") => {
        Some("Scroll viewport down half a page")
    };
    ("tui.altScreen.lineUp") => {
        Some("Scroll viewport up one line")
    };
    ("tui.altScreen.lineDown") => {
        Some("Scroll viewport down one line")
    };
    ("tui.altScreen.previousPrompt") => {
        Some("Jump to previous semantic prompt")
    };
    ("tui.altScreen.nextPrompt") => {
        Some("Jump to next semantic prompt")
    };
    ("tui.altScreen.search") => {
        Some("Search the primary scroll view")
    };
    ("tui.altScreen.searchNext") => {
        Some("Select the next search match")
    };
    ("tui.altScreen.searchPrevious") => {
        Some("Select the previous search match")
    };
    ("tui.altScreen.searchClose") => {
        Some("Close transcript search")
    };
    ("tui.altScreen.top") => {
        Some("Scroll viewport to top")
    };
    ("tui.altScreen.bottom") => {
        Some("Scroll viewport to bottom")
    };
}

/// Upstream `TUI_KEYBINDINGS` table (keys may be overridden by the user;
/// an empty default list means "unbound by default").
pub const TUI_KEYBINDINGS: &[KeybindingDefinition] = &[
    kb!("tui.editor.cursorUp", ["up"]),
    kb!("tui.editor.cursorDown", ["down"]),
    kb!("tui.editor.historyPrevious", []),
    kb!("tui.editor.historyNext", []),
    kb!("tui.editor.cursorLeft", ["left", "ctrl+b"]),
    kb!("tui.editor.cursorRight", ["right", "ctrl+f"]),
    kb!(
        "tui.editor.cursorWordLeft",
        ["alt+left", "ctrl+left", "alt+b"]
    ),
    kb!(
        "tui.editor.cursorWordRight",
        ["alt+right", "ctrl+right", "alt+f"]
    ),
    kb!(
        "tui.editor.cursorLineStart",
        ["home", "ctrl+home", "ctrl+a"]
    ),
    kb!("tui.editor.cursorLineEnd", ["end", "ctrl+end", "ctrl+e"]),
    kb!("tui.editor.jumpForward", ["ctrl+]"]),
    kb!("tui.editor.jumpBackward", ["ctrl+alt+]"]),
    kb!("tui.editor.pageUp", ["pageUp", "ctrl+pageUp"]),
    kb!("tui.editor.pageDown", ["pageDown", "ctrl+pageDown"]),
    kb!("tui.editor.deleteCharBackward", ["backspace"]),
    kb!("tui.editor.deleteCharForward", ["delete", "ctrl+d"]),
    kb!("tui.editor.deleteWordBackward", ["ctrl+w", "alt+backspace"]),
    kb!("tui.editor.deleteWordForward", ["alt+d", "alt+delete"]),
    kb!("tui.editor.deleteToLineStart", ["ctrl+u"]),
    kb!("tui.editor.deleteToLineEnd", ["ctrl+k"]),
    kb!("tui.editor.yank", ["ctrl+y"]),
    kb!("tui.editor.yankPop", ["alt+y"]),
    kb!("tui.editor.undo", ["ctrl+-"]),
    kb!("tui.input.newLine", ["shift+enter", "ctrl+j"]),
    kb!("tui.input.submit", ["enter"]),
    kb!("tui.input.tab", ["tab"]),
    kb!("tui.input.copy", ["ctrl+c"]),
    kb!("tui.select.up", ["up"]),
    kb!("tui.select.down", ["down"]),
    kb!("tui.select.pageUp", ["pageUp"]),
    kb!("tui.select.pageDown", ["pageDown"]),
    kb!("tui.select.confirm", ["enter"]),
    kb!("tui.select.cancel", ["escape", "ctrl+c"]),
    kb!("tui.altScreen.pageUp", ["pageUp"]),
    kb!("tui.altScreen.pageDown", ["pageDown"]),
    kb!("tui.altScreen.halfPageUp", []),
    kb!("tui.altScreen.halfPageDown", []),
    kb!("tui.altScreen.lineUp", []),
    kb!("tui.altScreen.lineDown", []),
    kb!("tui.altScreen.previousPrompt", ["ctrl+shift+up"]),
    kb!("tui.altScreen.nextPrompt", ["ctrl+shift+down"]),
    kb!("tui.altScreen.search", ["ctrl+shift+f"]),
    kb!("tui.altScreen.searchNext", ["enter", "ctrl+g"]),
    kb!(
        "tui.altScreen.searchPrevious",
        ["shift+enter", "ctrl+shift+g"]
    ),
    kb!("tui.altScreen.searchClose", ["escape"]),
    kb!("tui.altScreen.top", ["home"]),
    kb!("tui.altScreen.bottom", ["end"]),
];

/// A keybinding conflict: a single key bound to multiple user-set bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

/// User keybindings config: binding id -> keys (empty = unset).
pub type KeybindingsConfig = BTreeMap<String, Vec<String>>;

fn normalize_keys(keys: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for key in keys {
        if seen.insert(key.clone()) {
            result.push(key.clone());
        }
    }
    result
}

/// Registry managing default definitions plus user overrides.
#[derive(Clone)]
pub struct KeybindingsManager {
    definitions: Vec<KeybindingDefinition>,
    user_bindings: KeybindingsConfig,
    keys_by_id: BTreeMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    pub fn new(definitions: &[KeybindingDefinition], user_bindings: KeybindingsConfig) -> Self {
        let mut manager = Self {
            definitions: definitions.to_vec(),
            user_bindings,
            keys_by_id: BTreeMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    pub fn with_defaults(user_bindings: KeybindingsConfig) -> Self {
        Self::new(TUI_KEYBINDINGS, user_bindings)
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts = Vec::new();

        // Detect direct user-binding conflicts per key.
        let mut user_claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (keybinding, keys) in &self.user_bindings {
            if !self.definitions.iter().any(|d| d.id == keybinding) {
                continue;
            }
            let normalized = normalize_keys(keys);
            for key in normalized {
                user_claims
                    .entry(key)
                    .or_default()
                    .insert(keybinding.clone());
            }
        }
        for (key, keybindings) in user_claims {
            if keybindings.len() > 1 {
                self.conflicts.push(KeybindingConflict {
                    key,
                    keybindings: keybindings.into_iter().collect(),
                });
            }
        }

        for definition in &self.definitions {
            let user_keys = self.user_bindings.get(definition.id);
            let keys = match user_keys {
                Some(keys) => normalize_keys(keys),
                None => definition
                    .default_keys
                    .iter()
                    .map(|k| k.to_string())
                    .collect(),
            };
            self.keys_by_id.insert(definition.id.to_string(), keys);
        }
    }

    /// True when `key` matches any key bound to `binding`.
    pub fn matches(&self, key: &TuiKey, binding: Keybinding) -> bool {
        if let Some(keys) = self.keys_by_id.get(binding) {
            for pattern in keys {
                if match_key(key, pattern) {
                    return true;
                }
            }
        }
        false
    }

    /// Return every action bound to `key`, in definition order.
    ///
    /// Binding conflicts are intentionally not resolved by eviction. Callers
    /// that have a priority order (for example an active search overlay) can
    /// therefore inspect all claims and choose the first applicable action.
    pub fn matching_bindings(&self, key: &TuiKey) -> Vec<String> {
        self.definitions
            .iter()
            .filter(|definition| self.matches(key, definition.id))
            .map(|definition| definition.id.to_string())
            .collect()
    }

    /// Dispatch the first registered action matching `key`.
    ///
    /// The callback returns `true` when it consumed the action. If it returns
    /// `false`, dispatch continues to the next matching action, which makes
    /// conflict handling explicit and deterministic for callers.
    pub fn dispatch<F>(&self, key: &TuiKey, mut handler: F) -> Option<String>
    where
        F: FnMut(&str) -> bool,
    {
        self.matching_bindings(key)
            .into_iter()
            .find(|action| handler(action.as_str()))
    }

    /// Match a raw terminal sequence using the same parser as the controller.
    pub fn matches_raw(&self, raw: &str, binding: Keybinding) -> bool {
        self.keys_by_id.get(binding).is_some_and(|keys| {
            keys.iter()
                .any(|pattern| crate::keys::matches_raw_key(raw, pattern))
        })
    }

    pub fn get_keys(&self, binding: Keybinding) -> Vec<String> {
        self.keys_by_id.get(binding).cloned().unwrap_or_default()
    }

    pub fn get_definition(&self, binding: Keybinding) -> Option<&KeybindingDefinition> {
        self.definitions.iter().find(|d| d.id == binding)
    }

    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        let mut resolved = KeybindingsConfig::new();
        for definition in &self.definitions {
            if let Some(keys) = self.keys_by_id.get(definition.id) {
                resolved.insert(definition.id.to_string(), keys.clone());
            }
        }
        resolved
    }
}

static GLOBAL_KEYBINDINGS: OnceLock<Mutex<KeybindingsManager>> = OnceLock::new();

/// Replace the process-wide registry used by components and controllers.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
pub fn set_keybindings(manager: KeybindingsManager) {
    let registry = GLOBAL_KEYBINDINGS
        .get_or_init(|| Mutex::new(KeybindingsManager::with_defaults(BTreeMap::new())));
    *registry.lock().expect("global keybindings mutex poisoned") = manager;
}

/// Clone the process-wide registry used by built-in components.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
pub fn get_keybindings() -> KeybindingsManager {
    GLOBAL_KEYBINDINGS
        .get_or_init(|| Mutex::new(KeybindingsManager::with_defaults(BTreeMap::new())))
        .lock()
        .expect("global keybindings mutex poisoned")
        .clone()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::keys::TuiKey;

    fn manager() -> KeybindingsManager {
        KeybindingsManager::with_defaults(KeybindingsConfig::new())
    }

    #[test]
    fn binds_ctrl_j_as_default_newline_alias() {
        let kb = manager();
        assert_eq!(
            kb.get_keys("tui.input.newLine"),
            vec!["shift+enter", "ctrl+j"]
        );
        // In the pi key-string surface, kitty "\x1b[106;5u" arrives as ctrl+j.
        assert!(kb.matches(&TuiKey::shift("enter"), "tui.input.newLine"));
        assert!(kb.matches(&TuiKey::ctrl("j"), "tui.input.newLine"));
    }

    #[test]
    fn binds_modified_and_unmodified_viewport_navigation() {
        let kb = manager();
        assert_eq!(
            kb.get_keys("tui.editor.cursorLineStart"),
            vec!["home", "ctrl+home", "ctrl+a"]
        );
        assert_eq!(
            kb.get_keys("tui.editor.cursorLineEnd"),
            vec!["end", "ctrl+end", "ctrl+e"]
        );
        assert_eq!(
            kb.get_keys("tui.editor.pageUp"),
            vec!["pageUp", "ctrl+pageUp"]
        );
        assert_eq!(
            kb.get_keys("tui.editor.pageDown"),
            vec!["pageDown", "ctrl+pageDown"]
        );
    }

    #[test]
    fn preserves_upstream_page_key_spelling_without_changing_matching() {
        let kb = manager();
        assert_eq!(
            kb.get_resolved_bindings().get("tui.editor.pageUp").cloned(),
            Some(vec!["pageUp".to_string(), "ctrl+pageUp".to_string()])
        );
        assert!(kb.matches(&TuiKey::simple("pageup"), "tui.editor.pageUp"));
        assert!(kb.matches(&TuiKey::ctrl("pageup"), "tui.editor.pageUp"));
    }

    #[test]
    fn leaves_prompt_history_unbound_by_default() {
        let kb = manager();
        assert!(kb.get_keys("tui.editor.historyPrevious").is_empty());
        assert!(kb.get_keys("tui.editor.historyNext").is_empty());
    }

    #[test]
    fn default_definitions_keep_upstream_help_descriptions() {
        let kb = manager();
        assert_eq!(
            kb.get_definition("tui.editor.cursorWordLeft")
                .and_then(|definition| definition.description),
            Some("Move cursor word left")
        );
        assert_eq!(
            kb.get_definition("tui.altScreen.searchClose")
                .and_then(|definition| definition.description),
            Some("Close transcript search")
        );
        assert!(TUI_KEYBINDINGS
            .iter()
            .all(|definition| definition.description.is_some()));
    }

    #[test]
    fn binds_unmodified_alt_screen_shortcuts() {
        let kb = manager();
        assert_eq!(kb.get_keys("tui.altScreen.pageUp"), vec!["pageUp"]);
        assert_eq!(kb.get_keys("tui.altScreen.pageDown"), vec!["pageDown"]);
        assert!(kb.get_keys("tui.altScreen.halfPageUp").is_empty());
        assert!(kb.get_keys("tui.altScreen.halfPageDown").is_empty());
        assert!(kb.get_keys("tui.altScreen.lineUp").is_empty());
        assert!(kb.get_keys("tui.altScreen.lineDown").is_empty());
        assert_eq!(
            kb.get_keys("tui.altScreen.previousPrompt"),
            vec!["ctrl+shift+up"]
        );
        assert_eq!(
            kb.get_keys("tui.altScreen.nextPrompt"),
            vec!["ctrl+shift+down"]
        );
        assert_eq!(kb.get_keys("tui.altScreen.search"), vec!["ctrl+shift+f"]);
        assert_eq!(
            kb.get_keys("tui.altScreen.searchNext"),
            vec!["enter", "ctrl+g"]
        );
        assert_eq!(
            kb.get_keys("tui.altScreen.searchPrevious"),
            vec!["shift+enter", "ctrl+shift+g"]
        );
        assert_eq!(kb.get_keys("tui.altScreen.searchClose"), vec!["escape"]);
        assert_eq!(kb.get_keys("tui.altScreen.top"), vec!["home"]);
        assert_eq!(kb.get_keys("tui.altScreen.bottom"), vec!["end"]);
    }

    #[test]
    fn does_not_evict_selector_confirm_when_input_submit_rebound() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.input.submit".to_string(),
            vec!["enter".to_string(), "ctrl+enter".to_string()],
        );
        let kb = KeybindingsManager::with_defaults(user);
        assert_eq!(kb.get_keys("tui.input.submit"), vec!["enter", "ctrl+enter"]);
        assert_eq!(kb.get_keys("tui.select.confirm"), vec!["enter"]);
    }

    #[test]
    fn does_not_evict_cursor_bindings_when_another_action_reuses_key() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.select.up".to_string(),
            vec!["up".to_string(), "ctrl+p".to_string()],
        );
        let kb = KeybindingsManager::with_defaults(user);
        assert_eq!(kb.get_keys("tui.select.up"), vec!["up", "ctrl+p"]);
        assert_eq!(kb.get_keys("tui.editor.cursorUp"), vec!["up"]);
    }

    #[test]
    fn reports_direct_user_binding_conflicts_without_evicting_defaults() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.submit".to_string(), vec!["ctrl+x".to_string()]);
        user.insert("tui.select.confirm".to_string(), vec!["ctrl+x".to_string()]);
        let kb = KeybindingsManager::with_defaults(user);
        assert_eq!(
            kb.get_conflicts(),
            vec![KeybindingConflict {
                key: "ctrl+x".to_string(),
                keybindings: vec![
                    "tui.input.submit".to_string(),
                    "tui.select.confirm".to_string()
                ],
            }]
        );
        assert_eq!(kb.get_keys("tui.editor.cursorLeft"), vec!["left", "ctrl+b"]);
    }
}
