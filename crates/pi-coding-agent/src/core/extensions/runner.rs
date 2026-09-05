//! Extension runner — port of
//! `packages/coding-agent/src/core/extensions/runner.ts`.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::core::extensions::types::{
    EntryRenderer, Extension, ExtensionContext, ExtensionError, ExtensionFlag,
    ExtensionHostActions, ExtensionRuntime, ExtensionShortcut, ExtensionUiContext, HandlerFn,
    MarkdownTransformContext, MarkdownTransformer, MessageRenderer, RegisteredTool,
    ResolvedCommand, SourceInfo, ToolExecutionRequest, ToolRenderCallRequest, ToolRenderContext,
    ToolRenderResultOptions, ToolRenderResultRequest, ToolUpdateFn,
};

pub use crate::core::extensions::types::InputEventResult;

/// Error listener closure (upstream `ExtensionErrorListener`).
pub type ExtensionErrorListener = Arc<dyn Fn(ExtensionError) + Send + Sync>;

/// Diagnostics reported for extension shortcuts/commands.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDiagnostic {
    pub warning: bool,
    pub message: String,
    pub path: Option<String>,
}

/// Keybinding ids reserved from extension shortcuts.
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
    fn builtin_by_key(&self) -> BTreeMap<String, (String, bool)> {
        let mut out = BTreeMap::new();
        for (keybinding, keys) in &self.bindings {
            let restricted = RESERVED_KEYBINDINGS.contains(&keybinding.as_str());
            for key in keys {
                let normalized = key.to_lowercase();
                if out
                    .get(&normalized)
                    .map(|(_, existing_restricted)| *existing_restricted && !restricted)
                    .unwrap_or(false)
                {
                    continue;
                }
                out.insert(normalized, (keybinding.clone(), restricted));
            }
        }
        out
    }
}

/// One resource path returned by an extension's `resources_discover` hook.
///
/// Upstream retains the owning extension path so the resource loader can
/// attach source metadata and resolve relative paths from the extension
/// directory. The legacy string vectors below remain for compatibility with
/// existing mode callers, while these entries carry the parity information.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredResource {
    pub path: String,
    pub extension_path: String,
    pub source_info: SourceInfo,
}

impl DiscoveredResource {
    pub fn resolved_path(&self, cwd: &str) -> String {
        // `resources_discover` uses resolvePath(..., { trim: true }) in the
        // upstream loader. Reuse the extension path resolver so resource
        // paths have the same tilde, file-URL, Unicode-space, and lexical
        // normalization behavior as extension entries, even when the target
        // has not been created yet.
        let base = self.source_info.base_dir.as_deref().unwrap_or(cwd);
        crate::core::extensions::loader::resolve_relative_path(self.path.trim(), base)
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceDiscovery {
    pub skill_paths: Vec<String>,
    pub prompt_paths: Vec<String>,
    pub theme_paths: Vec<String>,
    pub skill_resources: Vec<DiscoveredResource>,
    pub prompt_resources: Vec<DiscoveredResource>,
    pub theme_resources: Vec<DiscoveredResource>,
}

impl ResourceDiscovery {
    fn resolved_paths(
        paths: &[String],
        resources: &[DiscoveredResource],
        cwd: &str,
    ) -> Vec<String> {
        if resources.is_empty() {
            return paths.to_vec();
        }
        resources
            .iter()
            .map(|resource| resource.resolved_path(cwd))
            .collect()
    }

    pub fn resolved_skill_paths(&self, cwd: &str) -> Vec<String> {
        Self::resolved_paths(&self.skill_paths, &self.skill_resources, cwd)
    }

    pub fn resolved_prompt_paths(&self, cwd: &str) -> Vec<String> {
        Self::resolved_paths(&self.prompt_paths, &self.prompt_resources, cwd)
    }

    pub fn resolved_theme_paths(&self, cwd: &str) -> Vec<String> {
        Self::resolved_paths(&self.theme_paths, &self.theme_resources, cwd)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectTrustResult {
    pub result: Option<Value>,
    pub errors: Vec<ExtensionError>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExtensionCompactionResult {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    pub usage: Option<pi_ai::types::Usage>,
    pub details: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct SessionBeforeCompactResult {
    pub cancelled: bool,
    pub compaction: Option<ExtensionCompactionResult>,
}

/// Extension runner (upstream `ExtensionRunner`).
#[derive(Clone)]
pub struct ExtensionRunner {
    extensions: Vec<Extension>,
    runtime: Arc<Mutex<ExtensionRuntime>>,
    cwd: String,
    mode: String,
    has_ui: bool,
    error_listeners: Arc<Mutex<Vec<(u64, ExtensionErrorListener)>>>,
    next_listener_id: Arc<Mutex<u64>>,
    shortcut_diagnostics: Vec<ResourceDiagnostic>,
    command_diagnostics: Vec<ResourceDiagnostic>,
}

impl ExtensionRunner {
    pub fn new(
        extensions: Vec<Extension>,
        runtime: Arc<Mutex<ExtensionRuntime>>,
        cwd: String,
    ) -> Self {
        Self {
            extensions,
            runtime,
            cwd,
            mode: "print".to_string(),
            has_ui: false,
            error_listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: Arc::new(Mutex::new(0)),
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
        self.extensions
            .iter()
            .map(|extension| extension.path.clone())
            .collect()
    }

    pub fn create_context(&self) -> ExtensionContext {
        self.create_context_with_ui(false)
    }

    /// Construct a callback context. A synchronous callback on the RPC input
    /// loop must not wait for stdin, while callbacks running in a tool or
    /// explicitly spawned command worker may use blocking dialogs.
    pub fn create_context_with_ui(&self, blocking_allowed: bool) -> ExtensionContext {
        self.create_context_with_ui_for_tool(blocking_allowed, None)
    }

    fn create_context_with_ui_for_tool(
        &self,
        blocking_allowed: bool,
        tool_call_id: Option<&str>,
    ) -> ExtensionContext {
        let (ui, host) = self
            .runtime
            .lock()
            .map(|runtime| {
                (
                    runtime.ui_context(self.has_ui, blocking_allowed),
                    runtime.host_context_for(blocking_allowed, tool_call_id),
                )
            })
            .unwrap_or_else(|_| (ExtensionUiContext::default(), Default::default()));
        ExtensionContext {
            mode: self.mode.clone(),
            cwd: self.cwd.clone(),
            has_ui: self.has_ui,
            ui,
            host,
        }
    }

    fn active_error(&self, event: &str) -> Option<ExtensionError> {
        match self.runtime.lock() {
            Ok(runtime) => runtime.assert_active().err().map(|error| ExtensionError {
                extension_path: "<runtime>".to_string(),
                event: event.to_string(),
                error,
            }),
            Err(_) => Some(ExtensionError {
                extension_path: "<runtime>".to_string(),
                event: event.to_string(),
                error: "Extension runtime lock poisoned".to_string(),
            }),
        }
    }

    fn handler_error(&self, extension_path: &str, event: &str, error: String) -> ExtensionError {
        let error = ExtensionError {
            extension_path: extension_path.to_string(),
            event: event.to_string(),
            error,
        };
        self.emit_error(error.clone());
        error
    }

    fn call_handler(
        handler: &HandlerFn,
        context: &ExtensionContext,
        payload: &Value,
    ) -> Result<Option<Value>, String> {
        match catch_unwind(AssertUnwindSafe(|| handler(context, payload))) {
            Ok(result) => result,
            Err(payload) => Err(format!(
                "extension handler panicked: {}",
                panic_message(payload)
            )),
        }
    }

    fn call_renderer(
        renderer: &MessageRenderer,
        item: &Value,
        options: &Value,
    ) -> Result<Option<Value>, String> {
        match catch_unwind(AssertUnwindSafe(|| renderer(item, options))) {
            Ok(result) => result,
            Err(payload) => Err(format!(
                "extension renderer panicked: {}",
                panic_message(payload)
            )),
        }
    }

    fn call_entry_renderer(
        renderer: &EntryRenderer,
        item: &Value,
        options: &Value,
    ) -> Result<Option<Value>, String> {
        match catch_unwind(AssertUnwindSafe(|| renderer(item, options))) {
            Ok(result) => result,
            Err(payload) => Err(format!(
                "extension renderer panicked: {}",
                panic_message(payload)
            )),
        }
    }

    /// All registered tools from all extensions. The first registration by
    /// extension/load order wins, matching upstream.
    pub fn get_all_registered_tools(&self) -> Vec<RegisteredTool> {
        let mut seen = BTreeSet::new();
        let mut tools = Vec::new();
        for extension in &self.extensions {
            for name in extension.ordered_tool_names() {
                if seen.insert(name.clone()) {
                    if let Some(tool) = extension.tools.get(&name) {
                        tools.push(tool.clone());
                    }
                }
            }
        }
        tools
    }

    pub fn get_tool_definition(&self, tool_name: &str) -> Option<RegisteredTool> {
        self.get_all_registered_tools()
            .into_iter()
            .find(|tool| tool.name == tool_name)
    }

    /// Execute a live extension tool callback through the extension runtime.
    /// This is the narrow integration point used by the bridge; the broader
    /// agent-loop registration remains outside this leaf.
    pub fn execute_tool(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.execute_tool_internal(tool_name, tool_call_id, params, None, true)
    }

    /// Execute a live extension tool and forward each native `onUpdate` value
    /// immediately to the supplied host callback.
    pub fn execute_tool_with_updates(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        params: Value,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<Value, String> {
        self.execute_tool_internal(tool_name, tool_call_id, params, on_update, true)
    }

    /// Execute a tool whose arguments were already prepared by the agent
    /// adapter. This prevents the same upstream `prepareArguments` callback
    /// from running twice on the normal AgentTool path.
    pub(crate) fn execute_tool_prepared_with_updates(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        params: Value,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<Value, String> {
        self.execute_tool_internal(tool_name, tool_call_id, params, on_update, false)
    }

    fn execute_tool_internal(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        params: Value,
        on_update: Option<ToolUpdateFn>,
        apply_prepare_arguments: bool,
    ) -> Result<Value, String> {
        let tool = self
            .get_tool_definition(tool_name)
            .ok_or_else(|| format!("Extension tool not found: {tool_name}"))?;
        let execute = tool
            .execute
            .ok_or_else(|| format!("Extension tool has no execute callback: {tool_name}"))?;
        let params = if apply_prepare_arguments {
            if let Some(prepare_arguments) = &tool.prepare_arguments {
                match catch_unwind(AssertUnwindSafe(|| prepare_arguments(params))) {
                    Ok(params) => params,
                    Err(payload) => {
                        let error = self.handler_error(
                            &tool.source_info.path,
                            "tool_prepare_arguments",
                            format!(
                                "extension tool argument preparation panicked: {}",
                                panic_message(payload)
                            ),
                        );
                        return Err(error.error);
                    }
                }
            } else {
                params
            }
        } else {
            params
        };
        let request = ToolExecutionRequest {
            tool_call_id: tool_call_id.to_string(),
            params,
            context: self.create_context_with_ui_for_tool(true, Some(tool_call_id)),
            on_update,
        };
        match catch_unwind(AssertUnwindSafe(|| execute(request))) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                let error = self.handler_error(&tool.source_info.path, "tool_execution", error);
                Err(error.error)
            }
            Err(payload) => {
                let error = self.handler_error(
                    &tool.source_info.path,
                    "tool_execution",
                    format!("extension tool panicked: {}", panic_message(payload)),
                );
                Err(error.error)
            }
        }
    }

    /// Invoke a registered open-JSON `renderCall` callback. The renderer's
    /// return value is intentionally opaque to this extension layer and can be
    /// consumed by any mode-specific presentation adapter.
    pub fn render_tool_call(
        &self,
        tool_name: &str,
        args: Value,
        theme: Value,
        context: ToolRenderContext,
    ) -> Result<Option<Value>, String> {
        let tool = self
            .get_tool_definition(tool_name)
            .ok_or_else(|| format!("Extension tool not found: {tool_name}"))?;
        let Some(render_call) = tool.render_call else {
            return Ok(None);
        };
        match catch_unwind(AssertUnwindSafe(|| {
            render_call(ToolRenderCallRequest {
                args,
                theme,
                context,
            })
        })) {
            Ok(Ok(value)) => Ok(Some(value)),
            Ok(Err(error)) => Err(self
                .handler_error(&tool.source_info.path, "tool_render_call", error)
                .error),
            Err(payload) => Err(self
                .handler_error(
                    &tool.source_info.path,
                    "tool_render_call",
                    format!(
                        "extension tool renderCall panicked: {}",
                        panic_message(payload)
                    ),
                )
                .error),
        }
    }

    /// Invoke a registered open-JSON `renderResult` callback. The callback is
    /// supplied the result, expanded/partial flags, theme, and full render
    /// context matching the upstream contract.
    pub fn render_tool_result(
        &self,
        tool_name: &str,
        result: Value,
        options: ToolRenderResultOptions,
        theme: Value,
        context: ToolRenderContext,
    ) -> Result<Option<Value>, String> {
        let tool = self
            .get_tool_definition(tool_name)
            .ok_or_else(|| format!("Extension tool not found: {tool_name}"))?;
        let Some(render_result) = tool.render_result else {
            return Ok(None);
        };
        match catch_unwind(AssertUnwindSafe(|| {
            render_result(ToolRenderResultRequest {
                result,
                options,
                theme,
                context,
            })
        })) {
            Ok(Ok(value)) => Ok(Some(value)),
            Ok(Err(error)) => Err(self
                .handler_error(&tool.source_info.path, "tool_render_result", error)
                .error),
            Err(payload) => Err(self
                .handler_error(
                    &tool.source_info.path,
                    "tool_render_result",
                    format!(
                        "extension tool renderResult panicked: {}",
                        panic_message(payload)
                    ),
                )
                .error),
        }
    }

    /// All extension flags. The first registration by name wins.
    pub fn get_flags(&self) -> BTreeMap<String, ExtensionFlag> {
        let mut flags = BTreeMap::new();
        for extension in &self.extensions {
            for name in extension.ordered_flag_names() {
                if let Some(flag) = extension.flags.get(&name).cloned() {
                    flags.entry(name).or_insert(flag);
                }
            }
        }
        flags
    }

    pub fn set_flag_value(&self, name: &str, value: Value) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.set_flag_value(name, value);
        }
    }

    pub fn get_flag_values(&self) -> BTreeMap<String, Value> {
        self.runtime
            .lock()
            .map(|runtime| runtime.flag_values.clone())
            .unwrap_or_default()
    }

    pub fn has_handlers(&self, event_type: &str) -> bool {
        self.extensions.iter().any(|extension| {
            extension
                .handlers
                .get(event_type)
                .map(|handlers| !handlers.is_empty())
                .unwrap_or(false)
        })
    }

    /// Emit the upstream session-start lifecycle event.  The extension
    /// loader calls this after constructing a mode-scoped runner so startup
    /// and reload handlers see the same event shape as the JS runtime.
    pub fn emit_session_start(&self, reason: &str) -> Result<(), Vec<ExtensionError>> {
        self.emit_session_start_with_previous(reason, None)
    }

    pub fn emit_session_start_with_previous(
        &self,
        reason: &str,
        previous_session_file: Option<&str>,
    ) -> Result<(), Vec<ExtensionError>> {
        let mut payload = json!({"type": "session_start", "reason": reason});
        if let Some(previous_session_file) = previous_session_file {
            payload["previousSessionFile"] = Value::String(previous_session_file.to_string());
        }
        self.emit("session_start", &payload).map(|_| ())
    }

    /// Emit the upstream session-shutdown lifecycle event before invalidating
    /// the runtime.  Handlers must run while their captured context is still
    /// active so cleanup callbacks can release external resources cleanly.
    pub fn emit_session_shutdown(&self, reason: &str) -> Result<(), Vec<ExtensionError>> {
        self.emit_session_shutdown_with_target(reason, None)
    }

    pub fn emit_session_shutdown_with_target(
        &self,
        reason: &str,
        target_session_file: Option<&str>,
    ) -> Result<(), Vec<ExtensionError>> {
        let mut payload = json!({"type": "session_shutdown", "reason": reason});
        if let Some(target_session_file) = target_session_file {
            payload["targetSessionFile"] = Value::String(target_session_file.to_string());
        }
        self.emit("session_shutdown", &payload).map(|_| ())
    }

    /// Emit a cancellable session switch event. A `{ cancel: true }` result
    /// vetoes the replacement while undecided handlers fall through.
    pub fn emit_session_before_switch(
        &self,
        reason: &str,
        target_session_file: Option<&str>,
    ) -> Result<bool, Vec<ExtensionError>> {
        let mut payload = json!({"type": "session_before_switch", "reason": reason});
        if let Some(target_session_file) = target_session_file {
            payload["targetSessionFile"] = Value::String(target_session_file.to_string());
        }
        self.emit("session_before_switch", &payload)
            .map(|result| result.is_some_and(|value| value["cancel"] == Value::Bool(true)))
    }

    /// Emit the upstream cancellable fork hook before the target entry is
    /// validated or the outgoing session is persisted.  The generic emitter
    /// stops at the first `{cancel:true}` result for all session-before
    /// events, preserving extension registration order.
    pub fn emit_session_before_fork(
        &self,
        entry_id: &str,
        position: &str,
    ) -> Result<bool, Vec<ExtensionError>> {
        let payload = json!({
            "type": "session_before_fork",
            "entryId": entry_id,
            "position": position,
        });
        self.emit("session_before_fork", &payload)
            .map(|result| result.is_some_and(|value| value["cancel"] == Value::Bool(true)))
    }

    /// Emit the cancellable compaction hook before summary generation.  Keep
    /// this at the runner boundary so interactive and RPC callers observe
    /// the same extension ordering and cancellation semantics.
    pub fn emit_session_before_compact(
        &self,
        payload: &Value,
    ) -> Result<SessionBeforeCompactResult, Vec<ExtensionError>> {
        let result = self.emit("session_before_compact", payload)?;
        let Some(value) = result else {
            return Ok(SessionBeforeCompactResult {
                cancelled: false,
                compaction: None,
            });
        };
        let cancelled = value.get("cancel") == Some(&Value::Bool(true));
        let compaction = match value.get("compaction").cloned() {
            Some(value) => Some(serde_json::from_value(value).map_err(|error| {
                vec![ExtensionError {
                    extension_path: "<session_before_compact>".to_string(),
                    event: "session_before_compact".to_string(),
                    error: format!("invalid compaction result: {error}"),
                }]
            })?),
            None => None,
        };
        Ok(SessionBeforeCompactResult {
            cancelled,
            compaction,
        })
    }

    fn resolved_commands(&self) -> Vec<ResolvedCommand> {
        let mut commands = Vec::new();
        for extension in &self.extensions {
            for name in extension.ordered_command_names() {
                if let Some(command) = extension.commands.get(&name) {
                    commands.push(command.clone());
                }
            }
        }
        let mut counts = BTreeMap::<String, usize>::new();
        for command in &commands {
            *counts.entry(command.name.clone()).or_default() += 1;
        }
        let mut seen = BTreeMap::<String, usize>::new();
        let mut taken = BTreeSet::new();
        commands
            .into_iter()
            .map(|command| {
                let occurrence = {
                    let count = seen.entry(command.name.clone()).or_default();
                    *count += 1;
                    *count
                };
                let mut invocation_name = if counts.get(&command.name).copied().unwrap_or(1) > 1 {
                    format!("{}:{occurrence}", command.name)
                } else {
                    command.name.clone()
                };
                if !taken.insert(invocation_name.clone()) {
                    let mut suffix = occurrence;
                    loop {
                        suffix += 1;
                        let candidate = format!("{}:{suffix}", command.name);
                        if taken.insert(candidate.clone()) {
                            invocation_name = candidate;
                            break;
                        }
                    }
                }
                ResolvedCommand {
                    name: command.name,
                    invocation_name,
                    source_info: command.source_info,
                    description: command.description,
                    handler: command.handler,
                }
            })
            .collect()
    }

    pub fn get_registered_commands(&mut self) -> Vec<ResolvedCommand> {
        self.command_diagnostics.clear();
        self.resolved_commands()
    }

    pub fn get_command_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.command_diagnostics.clone()
    }

    pub fn get_command(&mut self, name: &str) -> Option<ResolvedCommand> {
        self.get_registered_commands()
            .into_iter()
            .find(|command| command.invocation_name == name)
    }

    pub fn execute_command(
        &self,
        invocation_name: &str,
        args: &str,
    ) -> Result<Option<Value>, String> {
        self.execute_command_with_ui(invocation_name, args, false)
    }

    /// Execute a command from a worker-safe context. This is used by RPC mode
    /// so a command handler can block on the UI broker while the main loop
    /// continues reading `extension_ui_response` lines.
    pub fn execute_command_with_ui(
        &self,
        invocation_name: &str,
        args: &str,
        blocking_ui_allowed: bool,
    ) -> Result<Option<Value>, String> {
        let Some(command) = self
            .resolved_commands()
            .into_iter()
            .find(|command| command.invocation_name == invocation_name)
        else {
            return Err(format!("Extension command not found: {invocation_name}"));
        };
        let payload = json!({
            "type": "command",
            "name": command.name,
            "invocationName": invocation_name,
            "args": args,
        });
        match Self::call_handler(
            &command.handler,
            &self.create_context_with_ui(blocking_ui_allowed),
            &payload,
        ) {
            Ok(result) => Ok(result),
            Err(error) => {
                let error =
                    self.handler_error(&format!("command:{invocation_name}"), "command", error);
                Err(error.error)
            }
        }
    }

    /// Get shortcuts with built-in conflict diagnostics. Reserved built-ins
    /// skip extension shortcuts; non-reserved built-ins are intentionally
    /// allowed and diagnosed.
    pub fn get_shortcuts(
        &mut self,
        keybindings: &KeybindingsConfig,
    ) -> BTreeMap<String, ExtensionShortcut> {
        self.shortcut_diagnostics.clear();
        let builtin = keybindings.builtin_by_key();
        let mut shortcuts: BTreeMap<String, ExtensionShortcut> = BTreeMap::new();
        for extension in &self.extensions {
            for name in extension.ordered_shortcut_names() {
                let Some(shortcut) = extension.shortcuts.get(&name) else {
                    continue;
                };
                let normalized = name.to_lowercase();
                if let Some((binding, restricted)) = builtin.get(&normalized) {
                    if *restricted {
                        self.shortcut_diagnostics.push(ResourceDiagnostic {
                            warning: true,
                            message: format!(
                                "Extension shortcut '{name}' from {} conflicts with built-in shortcut '{binding}'. Skipping.",
                                shortcut.extension_path
                            ),
                            path: Some(shortcut.extension_path.clone()),
                        });
                        continue;
                    }
                    if !self.has_ui {
                        self.shortcut_diagnostics.push(ResourceDiagnostic {
                            warning: true,
                            message: format!(
                                "Extension shortcut conflict: '{name}' is built-in shortcut '{binding}' and {}. Using {}.",
                                shortcut.extension_path, shortcut.extension_path
                            ),
                            path: Some(shortcut.extension_path.clone()),
                        });
                    }
                }
                if let Some(existing) = shortcuts.get(&normalized) {
                    self.shortcut_diagnostics.push(ResourceDiagnostic {
                        warning: true,
                        message: format!(
                            "Extension shortcut conflict: '{name}' registered by both {} and {}. Using {}.",
                            existing.extension_path, shortcut.extension_path, shortcut.extension_path
                        ),
                        path: Some(shortcut.extension_path.clone()),
                    });
                }
                shortcuts.insert(normalized, shortcut.clone());
            }
        }
        shortcuts
    }

    pub fn get_shortcut_diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.shortcut_diagnostics.clone()
    }

    /// Register an error listener. Removal uses a stable id so removing one
    /// listener cannot shift or accidentally remove another listener.
    pub fn on_error(&self, listener: ExtensionErrorListener) -> Box<dyn Fn() + Send + Sync> {
        let id = match self.next_listener_id.lock() {
            Ok(mut next) => {
                let id = *next;
                *next = next.saturating_add(1);
                id
            }
            Err(_) => 0,
        };
        if let Ok(mut listeners) = self.error_listeners.lock() {
            listeners.push((id, listener));
        }
        let listeners = Arc::clone(&self.error_listeners);
        Box::new(move || {
            if let Ok(mut listeners) = listeners.lock() {
                listeners.retain(|(listener_id, _)| *listener_id != id);
            }
        })
    }

    pub fn emit_error(&self, error: ExtensionError) {
        let listeners = self
            .error_listeners
            .lock()
            .map(|listeners| {
                listeners
                    .iter()
                    .map(|(_, listener)| Arc::clone(listener))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for listener in listeners {
            let _ = catch_unwind(AssertUnwindSafe(|| listener(error.clone())));
        }
    }

    /// Emit a generic lifecycle event. All handlers run in extension and
    /// registration order. Session-before events stop only after a handler
    /// explicitly returns `{cancel:true}`; handler failures are isolated and
    /// reported to listeners.
    pub fn emit(
        &self,
        event_type: &str,
        payload: &Value,
    ) -> Result<Option<Value>, Vec<ExtensionError>> {
        let mut errors = Vec::new();
        if let Some(error) = self.active_error(event_type) {
            errors.push(error.clone());
            self.emit_error(error);
            return Err(errors);
        }
        let context = self.create_context();
        let session_before = event_type.starts_with("session_before_");
        let mut result = None;
        'extensions: for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get(event_type) else {
                continue;
            };
            for handler in handlers {
                match Self::call_handler(handler, &context, payload) {
                    Ok(Some(value)) => {
                        let cancel =
                            session_before && value.get("cancel") == Some(&Value::Bool(true));
                        result = Some(value);
                        if cancel {
                            break 'extensions;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(self.handler_error(&extension.path, event_type, error))
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(result)
        } else {
            Err(errors)
        }
    }

    pub fn get_message_renderer(&self, message_type: &str) -> Option<MessageRenderer> {
        for extension in &self.extensions {
            for name in extension.ordered_message_renderer_names() {
                if name == message_type {
                    if let Some(renderer) = extension.message_renderers.get(&name) {
                        return Some(Arc::clone(renderer));
                    }
                }
            }
        }
        None
    }

    pub fn render_message(
        &self,
        message_type: &str,
        message: &Value,
        options: &Value,
    ) -> Result<Option<Value>, String> {
        for extension in &self.extensions {
            for name in extension.ordered_message_renderer_names() {
                if name != message_type {
                    continue;
                }
                let Some(renderer) = extension.message_renderers.get(&name) else {
                    continue;
                };
                return match Self::call_renderer(renderer, message, options) {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        self.handler_error(&extension.path, "message_renderer", error);
                        Ok(None)
                    }
                };
            }
        }
        Ok(None)
    }

    pub fn get_entry_renderer(&self, entry_type: &str) -> Option<EntryRenderer> {
        for extension in &self.extensions {
            for name in extension.ordered_entry_renderer_names() {
                if name == entry_type {
                    if let Some(renderer) = extension.entry_renderers.get(&name) {
                        return Some(Arc::clone(renderer));
                    }
                }
            }
        }
        None
    }

    pub fn render_entry(
        &self,
        entry_type: &str,
        entry: &Value,
        options: &Value,
    ) -> Result<Option<Value>, String> {
        for extension in &self.extensions {
            for name in extension.ordered_entry_renderer_names() {
                if name != entry_type {
                    continue;
                }
                let Some(renderer) = extension.entry_renderers.get(&name) else {
                    continue;
                };
                return match Self::call_entry_renderer(renderer, entry, options) {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        self.handler_error(&extension.path, "entry_renderer", error);
                        Ok(None)
                    }
                };
            }
        }
        Ok(None)
    }

    pub fn get_markdown_transformers(&self) -> Vec<MarkdownTransformer> {
        self.extensions
            .iter()
            .filter_map(|extension| extension.markdown_transformer.clone())
            .collect()
    }

    pub fn apply_markdown_transformers(
        &self,
        markdown: &str,
        context: &MarkdownTransformContext,
    ) -> String {
        let mut current = markdown.to_string();
        for extension in &self.extensions {
            let Some(transformer) = &extension.markdown_transformer else {
                continue;
            };
            let result = catch_unwind(AssertUnwindSafe(|| transformer(&current, context)));
            match result {
                Ok(Ok(transformed)) => current = transformed,
                Ok(Err(error)) => {
                    self.handler_error(&extension.path, "markdown_transformer", error);
                }
                Err(payload) => {
                    self.handler_error(
                        &extension.path,
                        "markdown_transformer",
                        format!("extension transformer panicked: {}", panic_message(payload)),
                    );
                }
            }
        }
        current
    }

    pub fn emit_input(
        &self,
        text: &str,
        images: Option<Value>,
        source: &str,
        streaming_behavior: Option<&str>,
    ) -> InputEventResult {
        if let Some(error) = self.active_error("input") {
            self.emit_error(error);
            return InputEventResult::continue_with(text, images);
        }
        let original_text = text.to_string();
        let original_images = images.clone();
        let mut current_text = original_text.clone();
        let mut current_images = images;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("input") else {
                continue;
            };
            for handler in handlers {
                let event = json!({
                    "type": "input",
                    "text": current_text,
                    "images": current_images,
                    "source": source,
                    "streamingBehavior": streaming_behavior,
                });
                match Self::call_handler(handler, &context, &event) {
                    Ok(Some(result)) => match result.get("action").and_then(Value::as_str) {
                        Some("handled") => return InputEventResult::handled(),
                        Some("transform") => {
                            if let Some(transformed) = result.get("text").and_then(Value::as_str) {
                                current_text = transformed.to_string();
                            }
                            if result.get("images").is_some() {
                                current_images = result.get("images").cloned();
                            }
                        }
                        _ => {}
                    },
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "input", error);
                    }
                }
            }
        }
        if current_text == original_text && current_images == original_images {
            InputEventResult::continue_with(current_text, current_images)
        } else {
            InputEventResult::transform(current_text, current_images)
        }
    }

    pub fn emit_before_provider_headers(&self, headers: Value) -> Value {
        let mut current = headers;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("before_provider_headers") else {
                continue;
            };
            for handler in handlers {
                let event = json!({"type": "before_provider_headers", "headers": current});
                match Self::call_handler(handler, &context, &event) {
                    Ok(Some(patch)) => merge_headers(&mut current, patch),
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "before_provider_headers", error);
                    }
                }
            }
        }
        current
    }

    pub fn emit_context(&self, messages: Value) -> Value {
        let mut current = messages;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("context") else {
                continue;
            };
            for handler in handlers {
                let event = json!({"type": "context", "messages": current});
                match Self::call_handler(handler, &context, &event) {
                    Ok(Some(result)) => {
                        if let Some(messages) = result.get("messages") {
                            current = messages.clone();
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "context", error);
                    }
                };
            }
        }
        current
    }

    pub fn emit_tool_result(&self, result: Value) -> Option<Value> {
        let mut current = result;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("tool_result") else {
                continue;
            };
            for handler in handlers {
                let event = json!({"type": "tool_result", "result": current});
                match Self::call_handler(handler, &context, &event) {
                    Ok(Some(patch)) => apply_tool_result_patch(&mut current, &patch),
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "tool_result", error);
                    }
                };
            }
        }
        Some(current)
    }

    pub fn emit_before_agent_start(
        &self,
        prompt: &str,
        images: Option<Value>,
        system_prompt: &str,
        system_prompt_options: &Value,
    ) -> Option<Value> {
        let mut current_system_prompt = system_prompt.to_string();
        let mut messages = Vec::new();
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("before_agent_start") else {
                continue;
            };
            for handler in handlers {
                let event = json!({
                    "type": "before_agent_start",
                    "prompt": prompt,
                    "images": images,
                    "systemPrompt": current_system_prompt,
                    "systemPromptOptions": system_prompt_options,
                });
                match Self::call_handler(handler, &context, &event) {
                    Ok(Some(result)) => {
                        if let Some(system_prompt) =
                            result.get("systemPrompt").and_then(Value::as_str)
                        {
                            current_system_prompt = system_prompt.to_string();
                        }
                        if let Some(message) = result.get("message") {
                            messages.push(message.clone());
                        }
                        if let Some(additional) = result.get("messages").and_then(Value::as_array) {
                            messages.extend(additional.iter().cloned());
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "before_agent_start", error);
                    }
                };
            }
        }
        if messages.is_empty() && current_system_prompt == system_prompt {
            None
        } else {
            let mut output = Map::new();
            if !messages.is_empty() {
                output.insert("messages".to_string(), Value::Array(messages));
            }
            if current_system_prompt != system_prompt {
                output.insert(
                    "systemPrompt".to_string(),
                    Value::String(current_system_prompt),
                );
            }
            Some(Value::Object(output))
        }
    }

    pub fn emit_message_end(&self, message: Value) -> Value {
        let mut current = message;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("message_end") else {
                continue;
            };
            for handler in handlers {
                let event = json!({"type": "message_end", "message": current});
                match Self::call_handler(handler, &context, &event) {
                    Ok(Some(result)) => {
                        let candidate = result.get("message").cloned().unwrap_or(result);
                        if same_message_role(&current, &candidate) {
                            current = candidate;
                        } else {
                            self.handler_error(
                                &extension.path,
                                "message_end",
                                "message_end handlers must return a message with the same role"
                                    .to_string(),
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "message_end", error);
                    }
                };
            }
        }
        current
    }

    pub fn emit_tool_call(&self, event: &Value) -> Option<Value> {
        let mut result = None;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("tool_call") else {
                continue;
            };
            for handler in handlers {
                match Self::call_handler(handler, &context, event) {
                    Ok(Some(value)) => {
                        let block = value.get("block") == Some(&Value::Bool(true));
                        result = Some(value);
                        if block {
                            return result;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "tool_call", error);
                    }
                }
            }
        }
        result
    }

    pub fn emit_user_bash(&self, event: &Value) -> Option<Value> {
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("user_bash") else {
                continue;
            };
            for handler in handlers {
                match Self::call_handler(handler, &context, event) {
                    Ok(Some(value)) if !value.is_null() => return Some(value),
                    Ok(_) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "user_bash", error);
                    }
                }
            }
        }
        None
    }

    pub fn emit_before_provider_request(&self, request: Value) -> Value {
        let mut current = request;
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("before_provider_request") else {
                continue;
            };
            for handler in handlers {
                match Self::call_handler(handler, &context, &current) {
                    Ok(Some(value)) => current = value,
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "before_provider_request", error);
                    }
                }
            }
        }
        current
    }

    pub fn emit_resources_discover(&self, event: &Value) -> ResourceDiscovery {
        let mut resources = ResourceDiscovery::default();
        let context = self.create_context();
        for extension in &self.extensions {
            let Some(handlers) = extension.handlers.get("resources_discover") else {
                continue;
            };
            for handler in handlers {
                match Self::call_handler(handler, &context, event) {
                    Ok(Some(value)) => {
                        append_paths(&mut resources.skill_paths, value.get("skillPaths"));
                        append_paths(&mut resources.prompt_paths, value.get("promptPaths"));
                        append_paths(&mut resources.theme_paths, value.get("themePaths"));
                        append_discovered_resources(
                            &mut resources.skill_resources,
                            value.get("skillPaths"),
                            extension,
                        );
                        append_discovered_resources(
                            &mut resources.prompt_resources,
                            value.get("promptPaths"),
                            extension,
                        );
                        append_discovered_resources(
                            &mut resources.theme_resources,
                            value.get("themePaths"),
                            extension,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.handler_error(&extension.path, "resources_discover", error);
                    }
                }
            }
        }
        resources
    }

    pub fn invalidate(&self, message: Option<&str>) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.invalidate(message);
        }
    }

    pub fn bind_core(&self) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.bind_core();
        }
    }

    /// Bind host-owned action callbacks for the persistent external bridge.
    pub fn bind_core_with_actions(&self, actions: Arc<dyn ExtensionHostActions>) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.bind_core_with_actions(actions);
        }
    }

    pub fn extensions(&self) -> &[Extension] {
        &self.extensions
    }
}

/// Dispatch a project trust event. Undecided handlers do not stop later
/// handlers; the first explicit yes/no result wins and handler errors are
/// isolated.
pub fn emit_project_trust_event(runner: &ExtensionRunner, event: &Value) -> ProjectTrustResult {
    let mut output = ProjectTrustResult::default();
    let context = runner.create_context();
    for extension in &runner.extensions {
        let Some(handlers) = extension.handlers.get("project_trust") else {
            continue;
        };
        for handler in handlers {
            match ExtensionRunner::call_handler(handler, &context, event) {
                Ok(Some(value)) => {
                    let decided = value
                        .get("trusted")
                        .or_else(|| value.get("result"))
                        .and_then(Value::as_str)
                        .is_some_and(|decision| matches!(decision, "yes" | "no"));
                    if decided {
                        output.result = Some(value);
                        return output;
                    }
                }
                Ok(None) => {}
                Err(error) => output.errors.push(runner.handler_error(
                    &extension.path,
                    "project_trust",
                    error,
                )),
            }
        }
    }
    output
}

fn merge_headers(current: &mut Value, patch: Value) {
    let Some(current_headers) = current.as_object_mut() else {
        return;
    };
    let patch = patch.get("headers").cloned().unwrap_or(patch);
    if let Some(patch_headers) = patch.as_object() {
        for (key, value) in patch_headers {
            current_headers.insert(key.clone(), value.clone());
        }
    }
}

fn apply_tool_result_patch(current: &mut Value, patch: &Value) {
    let patch = patch.get("result").unwrap_or(patch);
    let Some(patch) = patch.as_object() else {
        return;
    };
    let Some(current) = current.as_object_mut() else {
        return;
    };
    for key in ["content", "details", "isError", "usage"] {
        if let Some(value) = patch.get(key) {
            current.insert(key.to_string(), value.clone());
        }
    }
}

fn same_message_role(current: &Value, candidate: &Value) -> bool {
    match (
        current.get("role").and_then(Value::as_str),
        candidate.get("role").and_then(Value::as_str),
    ) {
        (Some(current_role), Some(candidate_role)) => current_role == candidate_role,
        _ => true,
    }
}

fn append_paths(target: &mut Vec<String>, values: Option<&Value>) {
    if let Some(values) = values.and_then(Value::as_array) {
        for value in values {
            if let Some(path) = value.as_str() {
                target.push(path.to_string());
            }
        }
    }
}

fn append_discovered_resources(
    target: &mut Vec<DiscoveredResource>,
    values: Option<&Value>,
    extension: &Extension,
) {
    let mut source_info = extension.source_info.clone();
    // Resource entries use the extension label as their source rather than
    // the extension's own `local`/`user` loader source. This mirrors
    // `buildExtensionResourcePaths` and lets downstream loaders attribute a
    // discovered file to its owning extension while retaining its extension
    // directory as the relative-resolution base.
    source_info.source = if extension.path.starts_with('<') {
        format!("extension:{}", extension.path.trim_matches(['<', '>']))
    } else {
        let resolved_path = if extension.resolved_path.is_empty() {
            &extension.path
        } else {
            &extension.resolved_path
        };
        let name = std::path::Path::new(resolved_path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("extension");
        format!("extension:{name}")
    };
    source_info.scope = "temporary".to_string();
    source_info.origin = "top-level".to_string();
    if let Some(values) = values.and_then(Value::as_array) {
        for value in values {
            if let Some(path) = value.as_str() {
                target.push(DiscoveredResource {
                    path: path.to_string(),
                    extension_path: extension.path.clone(),
                    source_info: source_info.clone(),
                });
            }
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Source info helper (upstream `createSyntheticSourceInfo`).
pub fn synthetic_source_info(path: &str, source: &str, base_dir: Option<String>) -> SourceInfo {
    SourceInfo::synthetic(path, source, base_dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::extensions::types::{ExtensionShortcut, FlagType, RegisteredCommand};
    use std::time::Duration;

    fn tool(name: &str) -> RegisteredTool {
        RegisteredTool {
            name: name.to_string(),
            label: name.to_string(),
            description: format!("{name} tool"),
            parameters: Value::Object(Default::default()),
            source_info: SourceInfo::synthetic("ext", "local", None),
            execute: None,
            ..Default::default()
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
        Arc::new(|_, _| Ok(None))
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
        let mut e1 = Extension {
            path: "e1".into(),
            ..Default::default()
        };
        e1.tools.insert("bash".to_string(), tool("bash"));
        let mut e2 = Extension {
            path: "e2".into(),
            ..Default::default()
        };
        e2.tools.insert("bash".to_string(), tool("bash-other"));
        e2.tools.insert("custom".to_string(), tool("custom"));
        let runner = runner_with(vec![e1, e2]);
        let tools = runner.get_all_registered_tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool.name == "bash")
                .unwrap()
                .description,
            "bash tool"
        );
    }

    #[test]
    fn commands_disambiguated_with_name_suffix() {
        let mut e1 = Extension {
            path: "e1".into(),
            ..Default::default()
        };
        e1.commands
            .insert("dup".into(), command("dup", dummy_handler()));
        let mut e2 = Extension {
            path: "e2".into(),
            ..Default::default()
        };
        e2.commands
            .insert("dup".into(), command("dup", dummy_handler()));
        let mut runner = runner_with(vec![e1, e2]);
        let names: Vec<_> = runner
            .get_registered_commands()
            .into_iter()
            .map(|command| command.invocation_name)
            .collect();
        assert_eq!(names, vec!["dup:1", "dup:2"]);
    }

    #[test]
    fn flags_first_wins() {
        let mut extension = Extension {
            path: "e1".into(),
            ..Default::default()
        };
        extension.flags.insert(
            "artist".into(),
            crate::core::extensions::types::ExtensionFlag {
                name: "artist".into(),
                description: None,
                flag_type: FlagType::Boolean,
                default: Some(Value::Bool(false)),
                extension_path: "e1".into(),
            },
        );
        let runner = runner_with(vec![extension]);
        assert_eq!(runner.get_flags()["artist"].extension_path, "e1");
    }

    #[test]
    fn error_listener_unsubscribe_is_stable() {
        let runner = runner_with(Vec::new());
        let count = Arc::new(Mutex::new(0));
        let first = {
            let count = Arc::clone(&count);
            runner.on_error(Arc::new(move |_| {
                *count.lock().unwrap_or_else(|error| error.into_inner()) += 1
            }))
        };
        let second = {
            let count = Arc::clone(&count);
            runner.on_error(Arc::new(move |_| {
                *count.lock().unwrap_or_else(|error| error.into_inner()) += 1
            }))
        };
        first();
        runner.emit_error(ExtensionError {
            extension_path: "e".into(),
            event: "x".into(),
            error: "one".into(),
        });
        second();
        assert_eq!(*count.lock().unwrap_or_else(|error| error.into_inner()), 1);
    }

    #[test]
    fn native_command_ui_context_uses_real_broker_and_rejects_sync_waits() {
        let handler: HandlerFn = Arc::new(|context, _event| {
            let selected = context.ui.select(
                "Choose",
                &["first".to_string(), "second".to_string()],
                Some(Duration::from_secs(1)),
            )?;
            Ok(Some(json!({"selected": selected})))
        });
        let mut extension = Extension {
            path: "ui-extension".into(),
            ..Default::default()
        };
        extension
            .commands
            .insert("ask".into(), command("ask", handler));
        let runtime = Arc::new(Mutex::new(ExtensionRuntime::new()));
        let host =
            Arc::new(crate::core::extensions::integration::ExtensionHostState::new(None, "medium"));
        host.set_ui_enabled(true);
        let mut runner = ExtensionRunner::new(vec![extension], runtime, "/tmp".into());
        runner.set_ui_context("rpc", true);
        runner.bind_core_with_actions(host.clone());
        let runner = Arc::new(runner);

        let worker_runner = runner.clone();
        let worker =
            std::thread::spawn(move || worker_runner.execute_command_with_ui("ask", "", true));
        let request = (0..500)
            .find_map(|_| {
                host.drain_ui_requests().into_iter().next().or_else(|| {
                    std::thread::sleep(Duration::from_millis(1));
                    None
                })
            })
            .expect("native command should emit a UI request");
        assert_eq!(request["method"], "select");
        let id = request["id"].as_str().expect("request id").to_string();
        assert_eq!(
            host.handle_ui_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "value": "second"
            })),
            crate::core::extensions::types::ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            worker.join().expect("native command worker"),
            Ok(Some(json!({"selected": "second"})))
        );

        let error = runner
            .execute_command("ask", "")
            .expect_err("synchronous command dispatch must not wait for stdin");
        assert!(error.contains(crate::core::extensions::types::BLOCKING_UI_UNAVAILABLE_MESSAGE));
        assert!(host.drain_ui_requests().is_empty());
    }

    #[test]
    fn reserved_shortcuts_are_skipped() {
        let mut extension = Extension {
            path: "e1".into(),
            ..Default::default()
        };
        extension.shortcuts.insert(
            "ctrl+c".into(),
            ExtensionShortcut {
                shortcut: "ctrl+c".into(),
                description: None,
                handler: dummy_handler(),
                extension_path: "e1".into(),
            },
        );
        let mut runner = runner_with(vec![extension]);
        let shortcuts = runner.get_shortcuts(&KeybindingsConfig {
            bindings: BTreeMap::from([("app.interrupt".into(), vec!["ctrl+c".into()])]),
        });
        assert!(shortcuts.is_empty());
        assert_eq!(runner.get_shortcut_diagnostics().len(), 1);
    }

    #[test]
    fn lifecycle_events_and_extension_relative_resources_match_upstream_shape() {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let event_handler = {
            let events = Arc::clone(&events);
            Arc::new(move |_: &ExtensionContext, event: &Value| {
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(None)
            }) as HandlerFn
        };
        let resources_handler = Arc::new(|_: &ExtensionContext, event: &Value| {
            assert_eq!(event["type"], "resources_discover");
            assert_eq!(event["cwd"], "/tmp/project");
            assert_eq!(event["reason"], "startup");
            Ok(Some(json!({
                "skillPaths": ["skills"],
                "promptPaths": ["prompts/default.md"],
                "themePaths": ["themes/dark.json"],
            })))
        }) as HandlerFn;
        let mut extension = Extension {
            path: "/tmp/project/.pi/extensions/example.js".into(),
            source_info: SourceInfo::synthetic(
                "/tmp/project/.pi/extensions/example.js",
                "local",
                Some("/tmp/project/.pi/extensions".into()),
            ),
            ..Default::default()
        };
        extension
            .handlers
            .insert("session_start".into(), vec![event_handler.clone()]);
        extension
            .handlers
            .insert("session_shutdown".into(), vec![event_handler]);
        extension
            .handlers
            .insert("resources_discover".into(), vec![resources_handler]);

        let runner = ExtensionRunner::new(
            vec![extension],
            Arc::new(Mutex::new(ExtensionRuntime::new())),
            "/tmp/project".into(),
        );
        runner.emit_session_start("startup").unwrap();
        let resources = runner.emit_resources_discover(&json!({
            "type": "resources_discover",
            "cwd": "/tmp/project",
            "reason": "startup",
        }));
        runner.emit_session_shutdown("quit").unwrap();

        assert_eq!(resources.skill_paths, vec!["skills"]);
        assert_eq!(resources.prompt_paths, vec!["prompts/default.md"]);
        assert_eq!(resources.theme_paths, vec!["themes/dark.json"]);
        assert_eq!(
            resources.skill_resources[0].extension_path,
            "/tmp/project/.pi/extensions/example.js"
        );
        assert_eq!(
            resources.skill_resources[0].resolved_path("/tmp/project"),
            "/tmp/project/.pi/extensions/skills"
        );
        assert_eq!(
            resources.skill_resources[0].source_info.source,
            "extension:example"
        );
        assert_eq!(
            resources.skill_resources[0].source_info.base_dir.as_deref(),
            Some("/tmp/project/.pi/extensions")
        );
        let events = events.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(events[0]["type"], "session_start");
        assert_eq!(events[0]["reason"], "startup");
        assert_eq!(events[1]["type"], "session_shutdown");
        assert_eq!(events[1]["reason"], "quit");
    }

    #[test]
    fn discovered_resources_use_trimmed_file_url_tilde_and_lexical_resolution() {
        let source = SourceInfo::synthetic(
            "/tmp/project/.pi/extensions/example.js",
            "local",
            Some("/tmp/project/.pi/extensions".into()),
        );
        let resource = |path: &str| DiscoveredResource {
            path: path.to_string(),
            extension_path: source.path.clone(),
            source_info: source.clone(),
        };
        assert_eq!(
            resource("  ./skills/../prompts  ").resolved_path("/tmp/project"),
            "/tmp/project/.pi/extensions/prompts"
        );
        assert_eq!(
            resource("file:///tmp/project/.pi/extensions/../themes/dark.json")
                .resolved_path("/tmp/project"),
            "/tmp/project/.pi/themes/dark.json"
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                resource("~/pi-resource").resolved_path("/tmp/project"),
                home.join("pi-resource").to_string_lossy()
            );
        }
    }

    #[test]
    fn session_before_fork_has_upstream_shape_and_stops_on_cancellation() {
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let first = {
            let events = Arc::clone(&events);
            Arc::new(move |_: &ExtensionContext, event: &Value| {
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(Some(json!({"cancel": true})))
            }) as HandlerFn
        };
        let second = {
            let events = Arc::clone(&events);
            Arc::new(move |_: &ExtensionContext, event: &Value| {
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(None)
            }) as HandlerFn
        };
        let mut extension = Extension {
            path: "fork-hook.js".into(),
            ..Default::default()
        };
        extension
            .handlers
            .insert("session_before_fork".into(), vec![first, second]);

        let runner = runner_with(vec![extension]);
        assert!(runner.emit_session_before_fork("entry-1", "at").unwrap());
        assert_eq!(
            *events.lock().unwrap_or_else(|error| error.into_inner()),
            vec![json!({
                "type": "session_before_fork",
                "entryId": "entry-1",
                "position": "at",
            })]
        );
    }

    #[test]
    fn session_before_compact_preserves_payload_and_decodes_custom_result() {
        let observed = Arc::new(Mutex::new(None::<Value>));
        let handler = {
            let observed = Arc::clone(&observed);
            Arc::new(move |_: &ExtensionContext, event: &Value| {
                *observed.lock().unwrap_or_else(|error| error.into_inner()) = Some(event.clone());
                Ok(Some(json!({
                    "compaction": {
                        "summary": "extension summary",
                        "firstKeptEntryId": "kept-1",
                        "tokensBefore": 42,
                        "details": {"readFiles": ["README.md"], "modifiedFiles": []}
                    }
                })))
            }) as HandlerFn
        };
        let mut extension = Extension {
            path: "compact-hook.js".into(),
            ..Default::default()
        };
        extension
            .handlers
            .insert("session_before_compact".into(), vec![handler]);
        let runner = runner_with(vec![extension]);
        let payload = json!({
            "type": "session_before_compact",
            "preparation": {"tokensBefore": 42},
            "branchEntries": [{"type": "message", "id": "kept-1"}],
            "customInstructions": "focus",
            "reason": "manual",
            "willRetry": false,
        });

        let result = runner.emit_session_before_compact(&payload).unwrap();
        assert!(!result.cancelled);
        let compaction = result.compaction.expect("custom compaction result");
        assert_eq!(compaction.summary, "extension summary");
        assert_eq!(compaction.first_kept_entry_id, "kept-1");
        assert_eq!(compaction.tokens_before, 42);
        assert_eq!(
            *observed.lock().unwrap_or_else(|error| error.into_inner()),
            Some(payload)
        );
    }
}
