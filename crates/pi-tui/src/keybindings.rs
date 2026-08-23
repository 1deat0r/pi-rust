//! Global keybinding registry — port of `packages/tui/src/keybindings.ts`.
//!
//! Downstream packages can override bindings through a `KeybindingsManager`
//! constructed with the default definitions plus user overrides.

use std::collections::{BTreeMap, BTreeSet};

use crate::keys::{match_key, TuiKey};

/// A named keybinding ("tui.editor.cursorUp", ...).
pub type Keybinding = &'static str;

/// A single key id ("enter", "ctrl+c", "shift+enter", ...).
pub type KeyId = &'static str;

/// Default keybinding definitions, mirroring upstream `TUI_KEYBINDINGS`.
#[derive(Clone)]
pub struct KeybindingDefinition {
    pub id: Keybinding,
    pub default_keys: &'static [KeyId],
    pub description: Option<&'static str>,
}

macro_rules! kb {
    ($id:expr, [$($k:expr),*]) => {
        KeybindingDefinition { id: $id, default_keys: &[$($k),*], description: None }
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
    kb!("tui.editor.pageUp", ["pageup", "ctrl+pageup"]),
    kb!("tui.editor.pageDown", ["pagedown", "ctrl+pagedown"]),
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
    kb!("tui.select.pageUp", ["pageup"]),
    kb!("tui.select.pageDown", ["pagedown"]),
    kb!("tui.select.confirm", ["enter"]),
    kb!("tui.select.cancel", ["escape", "ctrl+c"]),
    kb!("tui.altScreen.pageUp", ["pageup"]),
    kb!("tui.altScreen.pageDown", ["pagedown"]),
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
                if !keys.is_empty() {
                    resolved.insert(definition.id.to_string(), keys.clone());
                }
            }
        }
        resolved
    }
}

#[cfg(test)]
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
            vec!["pageup", "ctrl+pageup"]
        );
        assert_eq!(
            kb.get_keys("tui.editor.pageDown"),
            vec!["pagedown", "ctrl+pagedown"]
        );
    }

    #[test]
    fn leaves_prompt_history_unbound_by_default() {
        let kb = manager();
        assert!(kb.get_keys("tui.editor.historyPrevious").is_empty());
        assert!(kb.get_keys("tui.editor.historyNext").is_empty());
    }

    #[test]
    fn binds_unmodified_alt_screen_shortcuts() {
        let kb = manager();
        assert_eq!(kb.get_keys("tui.altScreen.pageUp"), vec!["pageup"]);
        assert_eq!(kb.get_keys("tui.altScreen.pageDown"), vec!["pagedown"]);
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
