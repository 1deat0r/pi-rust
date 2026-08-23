//! Extension runner — port of
//! `packages/coding-agent/src/core/extensions/runner.ts`.
//!
//! The runner owns the loaded extension set and exposes the registration
//! aggregation and query surface used by the rest of the agent: registered
//! tools (first registration per name wins), commands (with invocation-name
//! disambiguation), flags, shortcuts and their conflict diagnostics, and
//! handler-presence checks. Event dispatch (`emit*`) is modeled structurally:
//! handler closures receive a JSON payload; with no live JS extensions the
//! dispatch loop's contract is tested via injected closures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::core::extensions::types::{
    Extension, ExtensionContext, ExtensionError, ExtensionFlag, ExtensionRuntime,
    RegisteredCommand, RegisteredTool, ResolvedCommand, SourceInfo,
};

/// Error listener closure (upstream `ExtensionErrorListener`).
pub type ExtensionErrorListener = Arc<dyn Fn(ExtensionError) + Send + Sync>;

/// Diagnostics reported for extension shortcuts/commands (upstream
/// `ResourceDiagnostic`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDiagnostic {
    pub warning: bool,
    pub message: String,
    pub path: Option<String>,
}

/// Keybinding ids reserved from extension shortcuts (upstream
/// `RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`).
pub const RESERVED_KEYBINDINGS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

#[derive(Debug, Clone, Default)]
pub struct KeybindingsConfig {
    /// keybinding id -> key(s)
    pub bindings: BTreeMap<String, Vec<String>>,
}

impl KeybindingsConfig {
    /// The per-key map with reserved-action precedence (upstream
    /// `buildBuiltinKeybindings`).
    fn builtin_by_key(&self) -> BTreeMap<String, (String, bool)> {
        let mut out: BTreeMap<String, (String, bool)> = BTreeMap::new();
        for (keybinding, keys) in &self.bindings {
            let restrict_override = RESERVED_KEYBINDINGS.contains(&keybinding.as_str());
            for key in keys {
                let normalized = key.to_lowercase();
                let existing = out.get(&normalized);
                if let Some((_binding, existing_restrict)) = existing {
                    if *existing_restrict && !restrict_override {
                        continue;
                    }
                }
                out.insert(normalized, (keybinding.clone(), restrict_override));
            }
        }
        out
    }
}

/// Extension runner (upstream `ExtensionRunner`).
#[derive(Clone)]
pub struct ExtensionRunner {
    extensions: Vec<Extension>,
    runtime: Arc<Mutex<ExtensionRuntime>>,
    cwd: String,
    mode: String,
    has_ui: bool,
    error_listeners: Arc<Mutex<Vec<ExtensionErrorListener>>>,
    shortcut_diagnostics: Vec<ResourceDiagnostic>,
    command_diagnostics: Vec<ResourceDiagnostic>,
}

impl ExtensionRunner {
    pub fn new(
        extensions: Vec<Extension>,
        runtime: Arc<Mutex<ExtensionRuntime>>,
        cwd: String,
    ) -> Self {
        let has_ui = false;
        let mode = "print".to_string();
        Self {
            extensions,
            runtime,
            cwd,
            mode,
            has_ui,
            error_listeners: Arc::new(Mutex::new(Vec::new())),
            shortcut_diagnostics: Vec::new(),
            command_diagnostics: Vec::new(),
        }
    }

    pub fn set_ui_context(&mut self, mode: &str, has_ui: bool) {
        self.mode = mode.to_string();
        self.has_ui = has_ui;
    }

    pub fn has_ui(&self) -> bool {
        self.has_ui
    }

    pub fn get_extension_paths(&self) -> Vec<String> {
        self.extensions.iter().map(|e| e.path.clone()).collect()
    }

    /// All registered tools from all extensions (first registration per name
    /// wins; upstream `getAllRegisteredTools`).
    pub fn get_all_registered_tools(&self) -> Vec<RegisteredTool> {
        let mut tools_by_name: BTreeMap<String, RegisteredTool> = BTreeMap::new();
        for ext in &self.extensions {
            for (name, tool) in &ext.tools {
                if !tools_by_name.contains_key(name) {
                    tools_by_name.insert(name.clone(), tool.clone());
                }
            }
        }
        tools_by_name.into_values().collect()
    }

    /// Get a tool definition by name (upstream `getToolDefinition`).
    pub fn get_tool_definition(&self, tool_name: &str) -> Option<RegisteredTool> {
        self.get_all_registered_tools()
            .into_iter()
            .find(|t| t.name == tool_name)
    }

    /// All extension flags (first registration per name wins; upstream
    /// `getFlags`).
    pub fn get_flags(&self) -> BTreeMap<String, ExtensionFlag> {
        let mut all_flags = BTreeMap::new();
        for ext in &self.extensions {
            for (name, flag) in &ext.flags {
                if !all_flags.contains_key(name) {
                    all_flags.insert(name.clone(), flag.clone());
                }
            }
        }
        all_flags
    }

    pub fn set_flag_value(&self, name: &str, value: Value) {
        self.runtime
            .lock()
            .unwrap()
            .flag_values
            .insert(name.to_string(), value);
    }

    pub fn get_flag_values(&self) -> BTreeMap<String, Value> {
        self.runtime.lock().unwrap().flag_values.clone()
    }

    /// Whether any extension registered a handler for an event type
    /// (upstream `hasHandlers`).
    pub fn has_handlers(&self, event_type: &str) -> bool {
        self.extensions.iter().any(|ext| {
            ext.handlers
                .get(event_type)
                .map(|h| !h.is_empty())
                .unwrap_or(false)
        })
    }

    /// Resolve registered commands with invocation-name disambiguation
    /// (upstream `resolveRegisteredCommands`): a name registered by multiple
    /// extensions gets `name`, `name:1`, `name:2`, ... suffixes, then unique
    /// suffixes for repeats.
    pub fn get_registered_commands(&mut self) -> Vec<ResolvedCommand> {
        self.command_diagnostics = Vec::new();
        let mut commands: Vec<RegisteredCommand> = Vec::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for ext in &self.extensions {
            for command in ext.commands.values() {
                commands.push(command.clone());
                *counts.entry(command.name.clone()).or_default() += 1;
            }
        }
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut taken: std::collections::BTreeSet<String> = BTreeSet::new();
        let mut resolved = Vec::new();
        for command in commands {
            let occurrence = *seen.entry(command.name.clone()).or_default() + 1;
            seen.insert(command.name.clone(), occurrence);
            let mut invocation_name = if counts.get(&command.name).copied().unwrap_or(1) > 1 {
                format!("{}:{occurrence}", command.name)
            } else {
                command.name.clone()
            };
            if taken.contains(&invocation_name) {
                let mut suffix = occurrence;
                loop {
                    suffix += 1;
                    invocation_name = format!("{}:{suffix}", command.name);
                    if !taken.contains(&invocation_name) {
                        break;
                    }
                }
            }
            taken.insert(invocation_name.clone());
            resolved.push(ResolvedCommand {
                name: command.name.clone(),
                invocation_name,
                source_info: command.source_info,
                description: command.description,
                handler: command.handler,
            });
        }
        resolved
    }

    pub fn get_command_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.command_diagnostics.clone()
    }

    pub fn get_command(&mut self, name: &str) -> Option<ResolvedCommand> {
        self.get_registered_commands()
            .into_iter()
            .find(|c| c.invocation_name == name)
    }

    /// Get shortcuts with built-in conflict diagnostics (upstream
    /// `getShortcuts`). Reserved keybindings skip extension shortcuts.
    pub fn get_shortcuts(
        &mut self,
        keybindings: &KeybindingsConfig,
    ) -> BTreeMap<String, crate::core::extensions::types::ExtensionShortcut> {
        self.shortcut_diagnostics = Vec::new();
        let builtin = keybindings.builtin_by_key();
        let mut extension_shortcuts: BTreeMap<
            String,
            crate::core::extensions::types::ExtensionShortcut,
        > = BTreeMap::new();
        for ext in &self.extensions {
            for (key, shortcut) in &ext.shortcuts {
                let normalized = key.to_lowercase();
                if let Some((_binding, restrict_override)) = builtin.get(&normalized) {
                    if *restrict_override {
                        self.shortcut_diagnostics.push(ResourceDiagnostic {
                            warning: true,
                            message: format!(
                                "Extension shortcut '{key}' from {} conflicts with built-in shortcut. Skipping.",
                                shortcut.extension_path
                            ),
                            path: Some(shortcut.extension_path.clone()),
                        });
                        continue;
                    }
                    self.shortcut_diagnostics.push(ResourceDiagnostic {
                        warning: true,
                        message: format!(
                            "Extension shortcut conflict: '{key}' is built-in shortcut and {}. Using {}.",
                            shortcut.extension_path, shortcut.extension_path
                        ),
                        path: Some(shortcut.extension_path.clone()),
                    });
                }
                if let Some(existing) = extension_shortcuts.get(&normalized) {
                    self.shortcut_diagnostics.push(ResourceDiagnostic {
                        warning: true,
                        message: format!(
                            "Extension shortcut conflict: '{key}' registered by both {} and {}. Using {}.",
                            existing.extension_path, shortcut.extension_path, shortcut.extension_path
                        ),
                        path: Some(shortcut.extension_path.clone()),
                    });
                }
                extension_shortcuts.insert(normalized, shortcut.clone());
            }
        }
        extension_shortcuts
    }

    pub fn get_shortcut_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.shortcut_diagnostics.clone()
    }

    /// Register an error listener; returns an unsubscribe closure.
    pub fn on_error(
        &self,
        listener: Arc<dyn Fn(ExtensionError) + Send + Sync>,
    ) -> Box<dyn Fn() + Send + Sync> {
        self.error_listeners.lock().unwrap().push(listener);
        let listeners = self.error_listeners.clone();
        let index = self.error_listeners.lock().unwrap().len() - 1;
        Box::new(move || {
            let mut guard = listeners.lock().unwrap();
            if index < guard.len() {
                guard.remove(index);
            }
        })
    }

    pub fn emit_error(&self, error: ExtensionError) {
        let listeners: Vec<ExtensionErrorListener> = self.error_listeners.lock().unwrap().clone();
        for listener in listeners {
            listener(error.clone());
        }
    }

    /// Emit an event to all handlers in registration order (upstream generic
    /// `emit`). Returns the first `cancel`-style result for session_before_*
    /// events (modeled here as the first non-None handler result).
    pub fn emit(
        &self,
        event_type: &str,
        payload: &Value,
    ) -> Result<Option<Value>, Vec<ExtensionError>> {
        let ctx = ExtensionContext {
            mode: self.mode.clone(),
            cwd: self.cwd.clone(),
            has_ui: self.has_ui,
        };
        let mut errors = Vec::new();
        for ext in &self.extensions {
            let handlers = ext.handlers.get(event_type);
            let Some(handlers) = handlers else { continue };
            for handler in handlers {
                match handler(&ctx, payload) {
                    Ok(Some(result)) => return Ok(Some(result)),
                    Ok(None) => {}
                    Err(error) => errors.push(ExtensionError {
                        extension_path: ext.path.clone(),
                        event: event_type.to_string(),
                        error,
                    }),
                }
            }
        }
        if errors.is_empty() {
            Ok(None)
        } else {
            Err(errors)
        }
    }

    /// Create an ExtensionContext for event handlers (upstream
    /// `createContext`).
    pub fn create_context(&self) -> ExtensionContext {
        ExtensionContext {
            mode: self.mode.clone(),
            cwd: self.cwd.clone(),
            has_ui: self.has_ui,
        }
    }

    /// Aggregate flags into a name->flag map for the loader default pass.
    pub fn extensions(&self) -> &[Extension] {
        &self.extensions
    }
}

/// Source info helper (upstream `createSyntheticSourceInfo`).
pub fn synthetic_source_info(path: &str, source: &str, base_dir: Option<String>) -> SourceInfo {
    SourceInfo::synthetic(path, source, base_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::types::ExtensionShortcut;
    use crate::core::extensions::types::HandlerFn;
    use serde_json::json;
    use std::sync::Arc as StdArc;

    fn tool(name: &str) -> RegisteredTool {
        RegisteredTool {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: Value::Object(Default::default()),
            source_info: SourceInfo::synthetic("ext", "local", None),
        }
    }

    fn command(name: &str, handler: HandlerFn) -> RegisteredCommand {
        RegisteredCommand {
            name: name.to_string(),
            source_info: SourceInfo::synthetic("ext", "local", None),
            description: None,
            handler,
        }
    }

    fn dummy_handler() -> HandlerFn {
        StdArc::new(
            |_ctx: &ExtensionContext, _payload: &Value| -> Result<Option<Value>, String> {
                Ok(None)
            },
        )
    }

    fn runner_with(extensions: Vec<Extension>) -> ExtensionRunner {
        ExtensionRunner::new(
            extensions,
            Arc::new(Mutex::new(ExtensionRuntime::new())),
            "/tmp".to_string(),
        )
    }

    #[test]
    fn tools_first_registration_wins() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.tools.insert("bash".to_string(), tool("bash"));
        let mut e2 = Extension::default();
        e2.path = "e2".into();
        e2.tools.insert("bash".to_string(), tool("bash-other"));
        e2.tools.insert("custom".to_string(), tool("custom"));
        let runner = runner_with(vec![e1, e2]);
        let tools = runner.get_all_registered_tools();
        assert_eq!(tools.len(), 2);
        let bash = tools.iter().find(|t| t.name == "bash").unwrap();
        assert_eq!(bash.description, "bash tool", "first registration wins");
    }

    #[test]
    fn get_tool_definition_by_name() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.tools.insert("t".to_string(), tool("t"));
        let runner = runner_with(vec![e1]);
        assert!(runner.get_tool_definition("t").is_some());
        assert!(runner.get_tool_definition("missing").is_none());
    }

    #[test]
    fn commands_disambiguated_with_name_suffix() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.commands
            .insert("dup".into(), command("dup", dummy_handler()));
        let mut e2 = Extension::default();
        e2.path = "e2".into();
        e2.commands
            .insert("dup".into(), command("dup", dummy_handler()));
        let mut runner = runner_with(vec![e1, e2]);
        let commands = runner.get_registered_commands();
        let names: Vec<String> = commands.iter().map(|c| c.invocation_name.clone()).collect();
        assert_eq!(names, vec!["dup:1", "dup:2"]);
        assert!(runner.get_command("dup:1").is_some());
        assert!(runner.get_command("dup:2").is_some());
        assert!(runner.get_command("dup").is_none());
    }

    #[test]
    fn single_command_keeps_invocation_name() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.commands
            .insert("solo".into(), command("solo", dummy_handler()));
        let mut runner = runner_with(vec![e1]);
        let commands = runner.get_registered_commands();
        assert_eq!(commands[0].invocation_name, "solo");
        assert!(runner.get_command("solo").is_some());
    }

    #[test]
    fn flags_first_wins_and_values() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.flags.insert(
            "artist".into(),
            ExtensionFlag {
                name: "artist".into(),
                description: None,
                flag_type: crate::core::extensions::types::FlagType::Boolean,
                default: Some(Value::Bool(false)),
                extension_path: "e1".into(),
            },
        );
        let mut e2 = Extension::default();
        e2.path = "e2".into();
        e2.flags.insert(
            "artist".into(),
            ExtensionFlag {
                name: "artist".into(),
                description: None,
                flag_type: crate::core::extensions::types::FlagType::Boolean,
                default: Some(Value::Bool(true)),
                extension_path: "e2".into(),
            },
        );
        let runner = runner_with(vec![e1, e2]);
        let flags = runner.get_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags["artist"].extension_path, "e1");
    }

    #[test]
    fn has_handlers_detects_registered_handlers() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.handlers
            .insert("agent_start".to_string(), vec![dummy_handler()]);
        let runner = runner_with(vec![e1]);
        assert!(runner.has_handlers("agent_start"));
        assert!(!runner.has_handlers("agent_end"));
    }

    #[test]
    fn emit_dispatches_to_handlers_and_collects_errors() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        let handler: HandlerFn =
            StdArc::new(|_ctx, payload: &Value| -> Result<Option<Value>, String> {
                let n = payload.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                if n > 0 {
                    Ok(Some(payload.clone()))
                } else {
                    Err("boom".to_string())
                }
            });
        e1.handlers.insert("turn_end".to_string(), vec![handler]);
        let runner = runner_with(vec![e1]);
        // Errored handler -> collected error.
        let result = runner.emit("turn_end", &json!({ "n": 0 }));
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].event, "turn_end");
        assert_eq!(errors[0].extension_path, "e1");
        // Returning handler -> first result returned.
        let result = runner.emit("turn_end", &json!({ "n": 1 })).unwrap();
        assert_eq!(result.unwrap()["n"], json!(1));
    }

    #[test]
    fn error_listeners_receive_errors() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.handlers.insert(
            "x".to_string(),
            vec![StdArc::new(|_ctx, _p| Err("fail".to_string()))],
        );
        let runner = runner_with(vec![e1]);
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let got = received.clone();
        let _unsub = runner.on_error(StdArc::new(move |error: ExtensionError| {
            got.lock().unwrap().push(error.error)
        }));
        // Direct emitError notifies listeners.
        runner.emit_error(ExtensionError {
            extension_path: "e".into(),
            event: "x".into(),
            error: "test".into(),
        });
        assert_eq!(received.lock().unwrap().len(), 1);
    }

    #[test]
    fn reserved_shortcuts_are_skipped_with_diagnostics() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.shortcuts.insert(
            "ctrl+c".into(),
            ExtensionShortcut {
                shortcut: "ctrl+c".into(),
                description: None,
                handler: dummy_handler(),
                extension_path: "e1".into(),
            },
        );
        let mut runner = runner_with(vec![e1]);
        let keybindings = KeybindingsConfig {
            bindings: BTreeMap::from([
                ("app.interrupt".to_string(), vec!["ctrl+c".to_string()]),
                ("app.clear".to_string(), vec!["ctrl+d".to_string()]),
            ]),
        };
        // ctrl+c is reserved (app.interrupt) -> skipped with a diagnostic.
        let shortcuts = runner.get_shortcuts(&keybindings);
        assert!(shortcuts.is_empty());
        let diagnostics = runner.get_shortcut_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0]
            .message
            .contains("conflicts with built-in shortcut"));
    }

    #[test]
    fn non_reserved_shortcuts_are_allowed() {
        let mut e1 = Extension::default();
        e1.path = "e1".into();
        e1.shortcuts.insert(
            "ctrl+alt+m".into(),
            ExtensionShortcut {
                shortcut: "ctrl+alt+m".into(),
                description: None,
                handler: dummy_handler(),
                extension_path: "e1".into(),
            },
        );
        let mut runner = runner_with(vec![e1]);
        let keybindings = KeybindingsConfig::default();
        let shortcuts = runner.get_shortcuts(&keybindings);
        assert_eq!(shortcuts.len(), 1);
        assert!(runner.get_shortcut_diagnostics().is_empty());
    }

    #[test]
    fn keybinding_builtin_map_reserved_wins() {
        let config = KeybindingsConfig {
            bindings: BTreeMap::from([
                ("app.interrupt".to_string(), vec!["ctrl+c".to_string()]),
                ("not.reserved".to_string(), vec!["ctrl+c".to_string()]),
            ]),
        };
        let builtin = config.builtin_by_key();
        let (binding, restrict) = &builtin["ctrl+c"];
        assert_eq!(binding, "app.interrupt");
        assert!(*restrict);
    }
}
