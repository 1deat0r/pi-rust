//! Extension system types — port of
//! `packages/coding-agent/src/core/extensions/types.ts`.
//!
//! Rust cannot execute TypeScript extension modules in-process, so the port
//! models the *observable surface* of the extension system: resolved
//! extension records, registered tools/commands/flags/shortcuts, the shared
//! runtime state (flag values, pending provider registrations, invalidation),
//! and the load-result/error shapes. Handlers and renderers are represented
//! as opaque closures over JSON payloads; the upstream `ExtensionAPI` surface
//! is provided so future Rust-native extensions can register through the same
//! API.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// Source metadata for an extension/command (port of `core/source-info.ts`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    pub base_dir: Option<String>,
}

impl SourceInfo {
    pub fn synthetic(path: &str, source: &str, base_dir: Option<String>) -> Self {
        Self {
            path: path.to_string(),
            source: source.to_string(),
            scope: "temporary".to_string(),
            origin: "top-level".to_string(),
            base_dir,
        }
    }
}

/// Event handler closure: receives the extension context and the event
/// payload, returns an optional result. The payload/result are opaque JSON so
/// handler dispatch stays generic (upstream `ExtensionHandler`).
pub type HandlerFn =
    Arc<dyn Fn(&ExtensionContext, &Value) -> Result<Option<Value>, String> + Send + Sync>;

/// A registered extension tool (upstream `RegisteredTool`).
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    /// Parameter schema (JSON Schema-ish value; upstream uses TypeBox).
    pub parameters: Value,
    pub source_info: SourceInfo,
}

/// A registered CLI flag (upstream `ExtensionFlag`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionFlag {
    pub name: String,
    pub description: Option<String>,
    pub flag_type: FlagType,
    pub default: Option<Value>,
    pub extension_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagType {
    Boolean,
    String,
}

/// A registered keyboard shortcut (upstream `ExtensionShortcut`).
#[derive(Clone)]
pub struct ExtensionShortcut {
    pub shortcut: String,
    pub description: Option<String>,
    pub handler: HandlerFn,
    pub extension_path: String,
}

/// A registered command (upstream `RegisteredCommand`).
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub source_info: SourceInfo,
    pub description: Option<String>,
    pub handler: HandlerFn,
}

/// A command with its invocation name resolved (upstream `ResolvedCommand`).
#[derive(Clone)]
pub struct ResolvedCommand {
    pub name: String,
    pub invocation_name: String,
    pub source_info: SourceInfo,
    pub description: Option<String>,
    pub handler: HandlerFn,
}

/// A loaded extension with all registered items (upstream `Extension`).
#[derive(Clone, Default)]
pub struct Extension {
    pub path: String,
    pub resolved_path: String,
    pub hidden: bool,
    pub source_info: SourceInfo,
    pub handlers: BTreeMap<String, Vec<HandlerFn>>,
    pub tools: BTreeMap<String, RegisteredTool>,
    pub message_renderers: BTreeMap<String, String>,
    pub entry_renderers: BTreeMap<String, String>,
    pub commands: BTreeMap<String, RegisteredCommand>,
    pub flags: BTreeMap<String, ExtensionFlag>,
    pub shortcuts: BTreeMap<String, ExtensionShortcut>,
}

impl std::fmt::Debug for Extension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Handlers are opaque closures; summarize the rest.
        f.debug_struct("Extension")
            .field("path", &self.path)
            .field("resolved_path", &self.resolved_path)
            .field("hidden", &self.hidden)
            .field("source_info", &self.source_info)
            .field("handler_events", &self.handlers.keys().collect::<Vec<_>>())
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("commands", &self.commands.keys().collect::<Vec<_>>())
            .field("flags", &self.flags.keys().collect::<Vec<_>>())
            .field("shortcuts", &self.shortcuts.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Result of loading extensions (upstream `LoadExtensionsResult`).
#[derive(Clone, Default)]
pub struct LoadExtensionsResult {
    pub extensions: Vec<Extension>,
    pub errors: Vec<ExtensionLoadError>,
    pub runtime: Arc<Mutex<ExtensionRuntime>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionLoadError {
    pub path: String,
    pub error: String,
}

/// Extension error emitted at runtime (upstream `ExtensionError`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
}
/// Extension context shared by handlers (upstream `ExtensionContext`,
/// reduced to the parts the port's runner can serve).
#[derive(Debug, Clone, Default)]
pub struct ExtensionContext {
    pub mode: String,
    pub cwd: String,
    pub has_ui: bool,
}

/// Shared runtime state created by the loader (upstream
/// `ExtensionRuntimeState` + `ExtensionRuntime`).
///
/// Action stubs throw until `runner.bind_core` replaces them; the Rust port
/// models this as an `initialized` flag plus queued provider registrations.
#[derive(Debug, Default)]
pub struct ExtensionRuntime {
    pub flag_values: BTreeMap<String, Value>,
    pub pending_provider_registrations: Vec<PendingProviderRegistration>,
    pub pending_native_provider_registrations: Vec<PendingNativeProviderRegistration>,
    initialized: bool,
    stale_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingProviderRegistration {
    pub name: String,
    pub config: Value,
    pub extension_path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingNativeProviderRegistration {
    pub provider: String,
    pub extension_path: String,
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark this extension instance stale (upstream `invalidate`). All
    /// further action/state access throws `assertActive` errors.
    pub fn invalidate(&mut self, message: Option<&str>) {
        if self.stale_message.is_none() {
            self.stale_message = Some(
                message
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| STALE_MESSAGE.to_string()),
            );
            // Drop queued registrations on invalidation (upstream unsubscribes
            // event-bus handlers; the port clears queued provider work).
            self.pending_provider_registrations.clear();
            self.pending_native_provider_registrations.clear();
        }
    }

    pub fn is_stale(&self) -> bool {
        self.stale_message.is_some()
    }

    /// Whether the runner bound concrete actions (upstream `bindCore`).
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn bind_core(&mut self) {
        self.initialized = true;
    }

    /// Set a CLI flag value (upstream `setFlagValue`).
    pub fn set_flag_value(&mut self, name: &str, value: Value) {
        self.flag_values.insert(name.to_string(), value);
    }
}

pub const STALE_MESSAGE: &str = "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload().";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_flag_values_default_and_override() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .flag_values
            .insert("no-tools".to_string(), Value::Bool(true));
        assert_eq!(
            runtime.flag_values.get("no-tools"),
            Some(&Value::Bool(true))
        );
        runtime.set_flag_value("no-tools", Value::Bool(false));
        assert_eq!(
            runtime.flag_values.get("no-tools"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn invalidate_marks_stale_and_clears_queued() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .pending_provider_registrations
            .push(PendingProviderRegistration {
                name: "demo".into(),
                config: Value::Null,
                extension_path: "ext".into(),
            });
        runtime.invalidate(None);
        assert!(runtime.is_stale());
        assert!(runtime.pending_provider_registrations.is_empty());
    }

    #[test]
    fn invalidate_once_keeps_first_message() {
        let mut runtime = ExtensionRuntime::new();
        runtime.invalidate(Some("first"));
        runtime.invalidate(Some("second"));
        assert!(runtime.is_stale());
    }

    #[test]
    fn bind_core_enables_initialized() {
        let mut runtime = ExtensionRuntime::new();
        assert!(!runtime.is_initialized());
        runtime.bind_core();
        assert!(runtime.is_initialized());
    }
}
