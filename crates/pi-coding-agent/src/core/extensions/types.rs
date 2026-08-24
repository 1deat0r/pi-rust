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

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
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

/// Renderer closures are the Rust-native equivalent of extension renderCall,
/// renderResult, and renderEntry callbacks. Their JSON payloads retain the
/// upstream open shape while their callback boundary is panic-safe in the
/// runner.
pub type MessageRenderer =
    Arc<dyn Fn(&Value, &Value) -> Result<Option<Value>, String> + Send + Sync>;
pub type RendererFn = MessageRenderer;
pub type EntryRenderer = Arc<dyn Fn(&Value, &Value) -> Result<Option<Value>, String> + Send + Sync>;
pub type MarkdownTransformer =
    Arc<dyn Fn(&str, &MarkdownTransformContext) -> Result<String, String> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkdownTransformContext {
    pub message_type: String,
    pub is_streaming: bool,
    pub available_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationKind {
    Handler,
    Tool,
    Command,
    Shortcut,
    Flag,
    MessageRenderer,
    MarkdownTransformer,
    EntryRenderer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationRecord {
    pub kind: RegistrationKind,
    pub name: Option<String>,
}

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
    pub message_renderers: BTreeMap<String, MessageRenderer>,
    pub entry_renderers: BTreeMap<String, EntryRenderer>,
    pub markdown_transformer: Option<MarkdownTransformer>,
    pub registrations: Vec<RegistrationRecord>,
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
            .field(
                "message_renderers",
                &self.message_renderers.keys().collect::<Vec<_>>(),
            )
            .field(
                "entry_renderers",
                &self.entry_renderers.keys().collect::<Vec<_>>(),
            )
            .field(
                "has_markdown_transformer",
                &self.markdown_transformer.is_some(),
            )
            .field("registrations", &self.registrations)
            .finish()
    }
}

impl Extension {
    pub fn record_registration(&mut self, kind: RegistrationKind, name: Option<String>) {
        self.registrations.push(RegistrationRecord { kind, name });
    }

    fn ordered_names(
        &self,
        kind: RegistrationKind,
        names: impl Iterator<Item = String>,
    ) -> Vec<String> {
        let mut ordered = Vec::new();
        let mut seen = BTreeSet::new();
        for registration in &self.registrations {
            if registration.kind == kind {
                if let Some(name) = &registration.name {
                    if seen.insert(name.clone()) {
                        ordered.push(name.clone());
                    }
                }
            }
        }
        for name in names {
            if seen.insert(name.clone()) {
                ordered.push(name);
            }
        }
        ordered
    }

    pub fn ordered_handler_events(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Handler, self.handlers.keys().cloned())
    }

    pub fn ordered_tool_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Tool, self.tools.keys().cloned())
    }

    pub fn ordered_command_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Command, self.commands.keys().cloned())
    }

    pub fn ordered_flag_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Flag, self.flags.keys().cloned())
    }

    pub fn ordered_shortcut_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Shortcut, self.shortcuts.keys().cloned())
    }

    pub fn ordered_message_renderer_names(&self) -> Vec<String> {
        self.ordered_names(
            RegistrationKind::MessageRenderer,
            self.message_renderers.keys().cloned(),
        )
    }

    pub fn ordered_entry_renderer_names(&self) -> Vec<String> {
        self.ordered_names(
            RegistrationKind::EntryRenderer,
            self.entry_renderers.keys().cloned(),
        )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    Continue,
    Transform,
    Handled,
    Consume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEventResult {
    pub action: InputAction,
    pub text: Option<String>,
    pub images: Option<Value>,
}

impl InputEventResult {
    pub fn continue_with(text: impl Into<String>, images: Option<Value>) -> Self {
        Self {
            action: InputAction::Continue,
            text: Some(text.into()),
            images,
        }
    }

    pub fn transform(text: impl Into<String>, images: Option<Value>) -> Self {
        Self {
            action: InputAction::Transform,
            text: Some(text.into()),
            images,
        }
    }

    pub fn consume() -> Self {
        Self {
            action: InputAction::Handled,
            text: None,
            images: None,
        }
    }

    pub fn handled() -> Self {
        Self::consume()
    }
}

/// Shared runtime state created by the loader (upstream
/// `ExtensionRuntimeState` + `ExtensionRuntime`).
///
/// Action stubs throw until `runner.bind_core` replaces them; the Rust port
/// models this as an `initialized` flag plus queued provider registrations.
#[derive(Default)]
pub struct ExtensionRuntime {
    pub flag_values: BTreeMap<String, Value>,
    pub pending_provider_registrations: Vec<PendingProviderRegistration>,
    pub pending_native_provider_registrations: Vec<PendingNativeProviderRegistration>,
    initialized: bool,
    stale_message: Option<String>,
    subscriptions: Arc<Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>>,
}

impl std::fmt::Debug for ExtensionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRuntime")
            .field("flag_values", &self.flag_values)
            .field(
                "pending_provider_registrations",
                &self.pending_provider_registrations,
            )
            .field(
                "pending_native_provider_registrations",
                &self.pending_native_provider_registrations,
            )
            .field("initialized", &self.initialized)
            .field("stale_message", &self.stale_message)
            .finish()
    }
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
            let subscriptions = self
                .subscriptions
                .lock()
                .map(|mut callbacks| std::mem::take(&mut *callbacks))
                .unwrap_or_default();
            for unsubscribe in subscriptions {
                let _ = catch_unwind(AssertUnwindSafe(|| unsubscribe()));
            }
        }
    }

    pub fn is_stale(&self) -> bool {
        self.stale_message.is_some()
    }

    /// Reject runtime actions after a session replacement or reload.
    pub fn assert_active(&self) -> Result<(), String> {
        match &self.stale_message {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }

    pub fn assert_initialized(&self) -> Result<(), String> {
        if self.initialized {
            Ok(())
        } else {
            Err(NOT_INITIALIZED_MESSAGE.to_string())
        }
    }

    pub fn stale_error(&self) -> Option<String> {
        self.stale_message.clone()
    }

    /// Track an event-bus subscription so session invalidation tears it down.
    /// The returned cleanup is idempotent from the caller's perspective; the
    /// runtime owns the authoritative invalidation cleanup.
    pub fn track_event_bus_subscription(
        &self,
        unsubscribe: Arc<dyn Fn() + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let called = Arc::clone(&called);
            move || {
                if !called.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    let _ = catch_unwind(AssertUnwindSafe(|| unsubscribe()));
                }
            }
        });
        if self.assert_active().is_ok() {
            if let Ok(mut callbacks) = self.subscriptions.lock() {
                callbacks.push(Arc::clone(&callback));
            }
        } else {
            callback();
        }
        Box::new(move || callback())
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
pub const NOT_INITIALIZED_MESSAGE: &str =
    "Extension runtime not initialized. Action methods cannot be called during extension loading.";

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
