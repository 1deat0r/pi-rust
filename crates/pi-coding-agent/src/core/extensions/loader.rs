//! Extension loader — port of
//! `packages/coding-agent/src/core/extensions/loader.ts`.
//!
//! Rust cannot execute TypeScript extension modules in-process (the upstream
//! uses jiti imports). The port keeps the exact discovery/resolution surface
//! and uses a persistent Node/Bun JSON-lines bridge for the supported external
//! runtime boundary. The bridge awaits the factory, returns registration
//! metadata, and keeps the JavaScript callbacks alive for command, hook, and
//! renderer calls. It deliberately does not claim to embed jiti, virtual
//! modules, or native pi-ai provider/action objects; those remain explicit
//! runtime-boundary limitations.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::config::CONFIG_DIR_NAME;
use crate::core::extensions::types::{
    EntryRenderer, Extension, ExtensionFlag, ExtensionLoadError, ExtensionRuntime,
    ExtensionShortcut, FlagType, HandlerFn, LoadExtensionsResult, MarkdownTransformer,
    MessageRenderer, PendingNativeProviderRegistration, PendingProviderRegistration,
    RegisteredCommand, RegisteredTool, RegistrationKind, SourceInfo,
};
use crate::core::pi_manifest::read_pi_manifest;

/// Rust-native equivalent of upstream `ExtensionAPI`. It is intentionally
/// scoped to registration: runtime actions are supplied by the runner after
/// loading and remain unavailable while a factory is evaluated.
pub struct ExtensionApi<'a> {
    extension: &'a mut Extension,
    runtime: Arc<Mutex<ExtensionRuntime>>,
    extension_path: String,
}

impl<'a> ExtensionApi<'a> {
    fn new(
        extension: &'a mut Extension,
        runtime: Arc<Mutex<ExtensionRuntime>>,
        extension_path: &str,
    ) -> Self {
        Self {
            extension,
            runtime,
            extension_path: extension_path.to_string(),
        }
    }

    fn assert_active(&self) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "Extension runtime lock poisoned".to_string())?
            .assert_active()
    }

    fn source_info(&self) -> SourceInfo {
        self.extension.source_info.clone()
    }

    pub fn on(&mut self, event: &str, handler: HandlerFn) -> Result<(), String> {
        self.assert_active()?;
        self.extension
            .handlers
            .entry(event.to_string())
            .or_default()
            .push(handler);
        self.extension
            .record_registration(RegistrationKind::Handler, Some(event.to_string()));
        Ok(())
    }

    pub fn register_tool(&mut self, tool: RegisteredTool) -> Result<(), String> {
        self.assert_active()?;
        let name = tool.name.clone();
        self.extension.tools.insert(name.clone(), tool);
        self.extension
            .record_registration(RegistrationKind::Tool, Some(name));
        Ok(())
    }

    pub fn register_command(
        &mut self,
        name: &str,
        description: Option<String>,
        handler: HandlerFn,
    ) -> Result<(), String> {
        self.assert_active()?;
        let name = name.to_string();
        self.extension.commands.insert(
            name.clone(),
            RegisteredCommand {
                name: name.clone(),
                source_info: self.source_info(),
                description,
                handler,
            },
        );
        self.extension
            .record_registration(RegistrationKind::Command, Some(name));
        Ok(())
    }

    pub fn register_shortcut(
        &mut self,
        shortcut: &str,
        description: Option<String>,
        handler: HandlerFn,
    ) -> Result<(), String> {
        self.assert_active()?;
        let shortcut_name = shortcut.to_string();
        self.extension.shortcuts.insert(
            shortcut_name.clone(),
            ExtensionShortcut {
                shortcut: shortcut_name.clone(),
                description,
                handler,
                extension_path: self.extension_path.clone(),
            },
        );
        self.extension
            .record_registration(RegistrationKind::Shortcut, Some(shortcut_name));
        Ok(())
    }

    pub fn register_flag(
        &mut self,
        name: &str,
        description: Option<String>,
        flag_type: FlagType,
        default: Option<serde_json::Value>,
    ) -> Result<(), String> {
        self.assert_active()?;
        if let Some(value) = &default {
            let valid = match flag_type {
                FlagType::Boolean => value.is_boolean(),
                FlagType::String => value.is_string(),
            };
            if !valid {
                let expected = match flag_type {
                    FlagType::Boolean => "boolean",
                    FlagType::String => "string",
                };
                let actual = match value {
                    serde_json::Value::Null => "object",
                    serde_json::Value::Bool(_) => "boolean",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => "object",
                };
                return Err(format!(
                    "Invalid default for flag \"{name}\": expected {expected}, got {actual}"
                ));
            }
        }
        let name = name.to_string();
        self.extension.flags.insert(
            name.clone(),
            ExtensionFlag {
                name: name.clone(),
                description,
                flag_type,
                default,
                extension_path: self.extension_path.clone(),
            },
        );
        if let Some(default) = self.extension.flags[&name].default.clone() {
            if let Ok(mut runtime) = self.runtime.lock() {
                runtime.flag_values.entry(name.clone()).or_insert(default);
            }
        }
        self.extension
            .record_registration(RegistrationKind::Flag, Some(name));
        Ok(())
    }

    pub fn register_message_renderer(
        &mut self,
        message_type: &str,
        renderer: MessageRenderer,
    ) -> Result<(), String> {
        self.assert_active()?;
        let name = message_type.to_string();
        self.extension
            .message_renderers
            .insert(name.clone(), renderer);
        self.extension
            .record_registration(RegistrationKind::MessageRenderer, Some(name));
        Ok(())
    }

    pub fn register_markdown_transformer(
        &mut self,
        transformer: MarkdownTransformer,
    ) -> Result<(), String> {
        self.assert_active()?;
        self.extension.markdown_transformer = Some(transformer);
        self.extension
            .record_registration(RegistrationKind::MarkdownTransformer, None);
        Ok(())
    }

    pub fn register_entry_renderer(
        &mut self,
        entry_type: &str,
        renderer: EntryRenderer,
    ) -> Result<(), String> {
        self.assert_active()?;
        let name = entry_type.to_string();
        self.extension
            .entry_renderers
            .insert(name.clone(), renderer);
        self.extension
            .record_registration(RegistrationKind::EntryRenderer, Some(name));
        Ok(())
    }

    pub fn get_flag(&self, name: &str) -> Result<Option<serde_json::Value>, String> {
        self.assert_active()?;
        Ok(self
            .runtime
            .lock()
            .map_err(|_| "Extension runtime lock poisoned".to_string())?
            .flag_values
            .get(name)
            .cloned())
    }

    /// Queue the JSON provider-config form of upstream `registerProvider`.
    /// Native provider objects are represented separately because their
    /// executable callbacks cannot cross the external bridge.
    pub fn register_provider(
        &mut self,
        name: &str,
        config: serde_json::Value,
    ) -> Result<(), String> {
        self.assert_active()?;
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Extension runtime lock poisoned".to_string())?;
        runtime
            .pending_provider_registrations
            .push(PendingProviderRegistration {
                name: name.to_string(),
                config,
                extension_path: self.extension_path.clone(),
            });
        Ok(())
    }

    /// Queue a deterministic native-provider identifier for Rust-native
    /// factories. JavaScript native provider callbacks are not serializable
    /// and are rejected by the external bridge rather than silently
    /// downgraded.
    pub fn register_native_provider(&mut self, provider: &str) -> Result<(), String> {
        self.assert_active()?;
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Extension runtime lock poisoned".to_string())?;
        runtime
            .pending_native_provider_registrations
            .push(PendingNativeProviderRegistration {
                provider: provider.to_string(),
                extension_path: self.extension_path.clone(),
            });
        Ok(())
    }

    pub fn unregister_provider(&mut self, name: &str) -> Result<(), String> {
        self.assert_active()?;
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Extension runtime lock poisoned".to_string())?;
        runtime
            .pending_provider_registrations
            .retain(|registration| registration.name != name);
        runtime
            .pending_native_provider_registrations
            .retain(|registration| registration.provider != name);
        Ok(())
    }
}

/// Entry file extensions accepted by the loader (upstream `isExtensionFile`).
pub fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js")
}

/// Resolve extension entry points from a directory. Checks, in order, for a
/// `package.json` with a `pi.extensions` field (returning its declared paths)
/// and then `index.ts`/`index.js`. Returns resolved paths or `None` when no
/// entry points exist.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json = dir.join("package.json");
    if package_json.exists() {
        if let Some(manifest) = read_pi_manifest(&package_json) {
            if !manifest.extensions.is_empty() {
                let mut entries = Vec::new();
                for ext_path in &manifest.extensions {
                    let resolved = dir.join(ext_path);
                    if resolved.exists() {
                        entries.push(resolved);
                    }
                }
                if !entries.is_empty() {
                    return Some(entries);
                }
            }
        }
    }
    let index_ts = dir.join("index.ts");
    let index_js = dir.join("index.js");
    if index_ts.exists() {
        return Some(vec![index_ts]);
    }
    if index_js.exists() {
        return Some(vec![index_js]);
    }
    None
}

/// Discover extensions in a directory (upstream `discoverExtensionsInDir`),
/// with no recursion beyond one level. The rules are: include direct
/// `*.ts`/`*.js` files; include subdirectory index files; include entries
/// declared by a subdirectory package.json `pi` manifest.
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut discovered = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().ok();
        let is_symlink = matches!(file_type, Some(t) if t.is_symlink());
        let is_file = matches!(file_type, Some(t) if t.is_file()) || is_symlink;
        let is_dir = matches!(file_type, Some(t) if t.is_dir()) || is_symlink;
        // 1. Direct files.
        if is_file && is_extension_file(&name) {
            discovered.push(entry_path);
            continue;
        }
        // 2 & 3. Subdirectories.
        if is_dir {
            if let Some(entries) = resolve_extension_entries(&entry_path) {
                discovered.extend(entries);
            }
        }
    }
    discovered
}

/// Bootstrap executed by a real Node/Bun runner. It is intentionally kept as
/// a data-only protocol: stdout carries JSON responses, while extension logs
/// are redirected to stderr so they cannot corrupt the protocol stream.
const EXTERNAL_EXTENSION_BRIDGE: &str = r###"
import { pathToFileURL } from "node:url";
import * as readline from "node:readline";

const entryPath = process.argv.at(-1);
const NOT_INITIALIZED = "Extension runtime not initialized. Action methods cannot be called during extension loading.";
const state = {
  active: true,
  staleMessage: undefined,
  handlers: new Map(),
  commands: new Map(),
  tools: new Map(),
  flags: new Map(),
  shortcuts: new Map(),
  messageRenderers: new Map(),
  entryRenderers: new Map(),
  markdownTransformer: undefined,
  providers: [],
  nativeProviders: [],
  registrations: [],
};

// A logging extension must not be able to write an invalid protocol frame.
console.log = (...args) => console.error(...args);
console.info = (...args) => console.error(...args);
console.debug = (...args) => console.error(...args);

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function assertActive() {
  if (!state.active) throw new Error(state.staleMessage ?? "Extension context is stale.");
}

function requireFunction(value, label) {
  if (typeof value !== "function") throw new Error(`${label} must be a function`);
}

function hasFunction(value, seen = new Set()) {
  if (typeof value === "function") return true;
  if (!value || typeof value !== "object" || seen.has(value)) return false;
  seen.add(value);
  return Object.values(value).some((child) => hasFunction(child, seen));
}

function record(kind, name) {
  state.registrations.push({ kind, name: name ?? null });
}

function addHandler(event, handler) {
  requireFunction(handler, `handler for ${event}`);
  const handlers = state.handlers.get(event) ?? [];
  handlers.push(handler);
  state.handlers.set(event, handlers);
  record("handler", event);
}

function notInitialized() {
  throw new Error(NOT_INITIALIZED);
}

const api = {
  on(event, handler) {
    assertActive();
    addHandler(event, handler);
  },
  registerTool(tool) {
    assertActive();
    if (!tool || typeof tool.name !== "string") throw new Error("Extension tools require a name");
    requireFunction(tool.execute, `execute for tool ${tool.name}`);
    state.tools.set(tool.name, {
      name: tool.name,
      description: tool.description ?? "",
      parameters: tool.parameters ?? {},
    });
    record("tool", tool.name);
  },
  registerCommand(name, options = {}) {
    assertActive();
    requireFunction(options.handler, `handler for command ${name}`);
    state.commands.set(name, { description: options.description ?? null, handler: options.handler });
    record("command", name);
  },
  registerShortcut(shortcut, options = {}) {
    assertActive();
    requireFunction(options.handler, `handler for shortcut ${shortcut}`);
    state.shortcuts.set(shortcut, { description: options.description ?? null, handler: options.handler });
    record("shortcut", shortcut);
  },
  registerFlag(name, options = {}) {
    assertActive();
    if (options.default !== undefined && typeof options.default !== options.type) {
      throw new Error(`Invalid default for flag "${name}": expected ${options.type}, got ${typeof options.default}`);
    }
    state.flags.set(name, {
      description: options.description ?? null,
      type: options.type,
      default: options.default,
    });
    record("flag", name);
  },
  registerMessageRenderer(customType, renderer) {
    assertActive();
    requireFunction(renderer, `message renderer for ${customType}`);
    state.messageRenderers.set(customType, renderer);
    record("message_renderer", customType);
  },
  registerMarkdownTransformer(transformer) {
    assertActive();
    requireFunction(transformer, "markdown transformer");
    state.markdownTransformer = transformer;
    record("markdown_transformer", null);
  },
  registerEntryRenderer(customType, renderer) {
    assertActive();
    requireFunction(renderer, `entry renderer for ${customType}`);
    state.entryRenderers.set(customType, renderer);
    record("entry_renderer", customType);
  },
  getFlag(name) {
    assertActive();
    return state.flags.has(name) ? state.flags.get(name).default : undefined;
  },
  registerProvider(nameOrProvider, config) {
    assertActive();
    if (typeof nameOrProvider !== "string") {
      throw new Error("External extension bridge does not support native provider callbacks");
    }
    if (config === undefined) throw new Error("Provider config is required when registering by name");
    if (hasFunction(config)) {
      throw new Error("External extension bridge only supports JSON provider configs");
    }
    state.providers.push({ name: nameOrProvider, config });
  },
  unregisterProvider(name) {
    assertActive();
    state.providers = state.providers.filter((registration) => registration.name !== name);
  },
  sendMessage: notInitialized,
  sendUserMessage: notInitialized,
  appendEntry: notInitialized,
  setSessionName: notInitialized,
  getSessionName: notInitialized,
  setLabel: notInitialized,
  getActiveTools: notInitialized,
  getAllTools: notInitialized,
  setActiveTools: notInitialized,
  getCommands: notInitialized,
  setModel: notInitialized,
  getThinkingLevel: notInitialized,
  setThinkingLevel: notInitialized,
};

function metadata() {
  return {
    handlers: [...state.handlers].map(([event, handlers]) => ({ event, count: handlers.length })),
    commands: [...state.commands].map(([name, value]) => ({ name, description: value.description })),
    tools: [...state.tools.values()],
    flags: [...state.flags].map(([name, value]) => ({ name, ...value })),
    shortcuts: [...state.shortcuts].map(([shortcut, value]) => ({ shortcut, ...value })),
    messageRenderers: [...state.messageRenderers.keys()],
    entryRenderers: [...state.entryRenderers.keys()],
    markdownTransformer: state.markdownTransformer !== undefined,
    providers: state.providers,
    nativeProviders: state.nativeProviders,
    registrations: state.registrations,
  };
}

function contextFor(request) {
  const context = { ...(request.context ?? {}) };
  if (request.kind === "handler" && request.name === "before_agent_start") {
    context.getSystemPrompt = () => request.event?.systemPrompt ?? "";
  }
  return context;
}

async function invoke(request) {
  assertActive();
  const context = contextFor(request);
  if (request.kind === "handler") {
    const handlers = state.handlers.get(request.name) ?? [];
    const handler = handlers[request.index];
    if (!handler) throw new Error(`Extension handler not found: ${request.name}[${request.index}]`);
    const result = await handler(request.event, context);
    // Upstream before_provider_headers handlers mutate the shared event and
    // their return value is ignored. Returning the event lets Rust apply the
    // same in-place mutation semantics across the process boundary.
    return request.name === "before_provider_headers" ? request.event : result;
  }
  if (request.kind === "command") {
    const command = state.commands.get(request.name);
    if (!command) throw new Error(`Extension command not found: ${request.name}`);
    return await command.handler(request.args ?? "", context);
  }
  if (request.kind === "shortcut") {
    const shortcut = state.shortcuts.get(request.name);
    if (!shortcut) throw new Error(`Extension shortcut not found: ${request.name}`);
    return await shortcut.handler(context);
  }
  if (request.kind === "message_renderer") {
    const renderer = state.messageRenderers.get(request.name);
    if (!renderer) throw new Error(`Extension message renderer not found: ${request.name}`);
    return await renderer(request.item, request.options, null);
  }
  if (request.kind === "entry_renderer") {
    const renderer = state.entryRenderers.get(request.name);
    if (!renderer) throw new Error(`Extension entry renderer not found: ${request.name}`);
    return await renderer(request.item, request.options, null);
  }
  if (request.kind === "markdown_transformer") {
    if (!state.markdownTransformer) throw new Error("Extension markdown transformer not found");
    return await state.markdownTransformer(request.markdown, request.context ?? {});
  }
  throw new Error(`Unknown extension bridge call: ${request.kind}`);
}

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

async function main() {
  if (!entryPath) throw new Error("Extension bridge did not receive an entry path");
  const module = await import(pathToFileURL(entryPath).href);
  const factory = module.default;
  if (typeof factory !== "function") {
    send({ type: "load_error", error: `Extension does not export a valid factory function: ${entryPath}` });
    process.exitCode = 1;
    return;
  }
  await factory(api);
  send({ type: "ready", ...metadata() });

  const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of input) {
    if (!line.trim()) continue;
    let request;
    try {
      request = JSON.parse(line);
      const result = await invoke(request);
      send({ id: request.id, ok: true, result: result === undefined ? null : result });
    } catch (error) {
      send({ id: request?.id ?? null, ok: false, error: errorMessage(error) });
    }
  }
}

main().catch((error) => {
  send({ type: "load_error", error: errorMessage(error) });
  process.exitCode = 1;
});
"###;

struct ExternalProcessState {
    child: std::process::Child,
    stdin: BufWriter<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: Arc<Mutex<String>>,
}

struct ExternalExtensionProcess {
    state: Mutex<ExternalProcessState>,
    next_request_id: AtomicU64,
}

impl ExternalExtensionProcess {
    fn request(&self, mut request: serde_json::Value) -> Result<Option<serde_json::Value>, String> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        request
            .as_object_mut()
            .ok_or_else(|| "Extension bridge request must be an object".to_string())?
            .insert("id".to_string(), serde_json::Value::from(id));
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Extension bridge lock poisoned".to_string())?;
        if let Some(status) = state
            .child
            .try_wait()
            .map_err(|error| format!("Extension bridge status failed: {error}"))?
        {
            return Err(format_child_exit(status, &state.stderr));
        }
        serde_json::to_writer(&mut state.stdin, &request)
            .map_err(|error| format!("Extension bridge write failed: {error}"))?;
        state
            .stdin
            .write_all(b"\n")
            .map_err(|error| format!("Extension bridge write failed: {error}"))?;
        state
            .stdin
            .flush()
            .map_err(|error| format!("Extension bridge flush failed: {error}"))?;

        let mut line = String::new();
        let read = state
            .stdout
            .read_line(&mut line)
            .map_err(|error| format!("Extension bridge read failed: {error}"))?;
        if read == 0 {
            return Err(format_child_exit_after_eof(&mut state));
        }
        let response: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|error| format!("Extension bridge returned invalid JSON: {error}"))?;
        if response.get("id") != Some(&serde_json::Value::from(id)) {
            return Err("Extension bridge returned an unexpected response id".to_string());
        }
        if response.get("ok") == Some(&serde_json::Value::Bool(false)) {
            return Err(response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Extension callback failed")
                .to_string());
        }
        let result = response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Ok((!result.is_null()).then_some(result))
    }
}

impl Drop for ExternalExtensionProcess {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let _ = state.stdin.write_all(b"{\"type\":\"close\"}\n");
            let _ = state.stdin.flush();
            let _ = state.child.kill();
            let _ = state.child.wait();
        }
    }
}

fn format_child_exit(status: std::process::ExitStatus, stderr: &Arc<Mutex<String>>) -> String {
    let detail = stderr
        .lock()
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match detail {
        Some(detail) => format!("Extension bridge exited: {detail}"),
        None => format!(
            "Extension bridge exited with code {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ),
    }
}

fn format_child_exit_after_eof(state: &mut ExternalProcessState) -> String {
    let status = state.child.try_wait().ok().flatten();
    match status {
        Some(status) => format_child_exit(status, &state.stderr),
        None => "Extension bridge closed stdout unexpectedly".to_string(),
    }
}

fn is_javascript_runtime(runner: &str) -> bool {
    Path::new(runner)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let name = name.to_ascii_lowercase();
            name == "node" || name == "node.exe" || name == "bun" || name == "bun.exe"
        })
        .unwrap_or(false)
}

fn bridge_context(context: &crate::core::extensions::types::ExtensionContext) -> serde_json::Value {
    serde_json::json!({
        "mode": context.mode,
        "cwd": context.cwd,
        "hasUI": context.has_ui,
    })
}

fn external_handler(
    process: Arc<ExternalExtensionProcess>,
    event: String,
    index: usize,
) -> HandlerFn {
    Arc::new(move |context, payload| {
        process.request(serde_json::json!({
            "type": "call",
            "kind": "handler",
            "name": event,
            "index": index,
            "event": payload,
            "context": bridge_context(context),
        }))
    })
}

fn external_command_handler(process: Arc<ExternalExtensionProcess>, name: String) -> HandlerFn {
    Arc::new(move |context, payload| {
        process.request(serde_json::json!({
            "type": "call",
            "kind": "command",
            "name": name,
            "args": payload.get("args").and_then(serde_json::Value::as_str).unwrap_or_default(),
            "context": bridge_context(context),
        }))
    })
}

fn external_shortcut_handler(process: Arc<ExternalExtensionProcess>, name: String) -> HandlerFn {
    Arc::new(move |context, _payload| {
        process.request(serde_json::json!({
            "type": "call",
            "kind": "shortcut",
            "name": name,
            "context": bridge_context(context),
        }))
    })
}

fn external_message_renderer(
    process: Arc<ExternalExtensionProcess>,
    name: String,
) -> MessageRenderer {
    Arc::new(move |item, options| {
        process.request(serde_json::json!({
            "type": "call",
            "kind": "message_renderer",
            "name": name,
            "item": item,
            "options": options,
        }))
    })
}

fn external_entry_renderer(process: Arc<ExternalExtensionProcess>, name: String) -> EntryRenderer {
    Arc::new(move |item, options| {
        process.request(serde_json::json!({
            "type": "call",
            "kind": "entry_renderer",
            "name": name,
            "item": item,
            "options": options,
        }))
    })
}

fn external_markdown_transformer(process: Arc<ExternalExtensionProcess>) -> MarkdownTransformer {
    Arc::new(move |markdown, context| {
        let result = process.request(serde_json::json!({
            "type": "call",
            "kind": "markdown_transformer",
            "markdown": markdown,
            "context": {
                "messageType": context.message_type,
                "isStreaming": context.is_streaming,
                "availableWidth": context.available_width,
            },
        }))?;
        Ok(result
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| markdown.to_string()))
    })
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_external_bridge(
    extension_path: &str,
    resolved_path: &Path,
    runner: &str,
    timeout_ms: Option<u64>,
) -> Result<(Arc<ExternalExtensionProcess>, serde_json::Value), String> {
    let cwd = resolved_path.parent().unwrap_or_else(|| Path::new("."));
    let mut command = Command::new(runner);
    if Path::new(runner)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase().starts_with("node"))
        .unwrap_or(false)
    {
        command.arg("--input-type=module");
    }
    command
        .arg("--eval")
        .arg(EXTERNAL_EXTENSION_BRIDGE)
        .arg("--")
        .arg(resolved_path)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to load extension: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to load extension: bridge stdin was not created".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to load extension: bridge stdout was not created".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to load extension: bridge stderr was not created".to_string())?;
    let stderr_capture = Arc::new(Mutex::new(String::new()));
    let stderr_for_thread = Arc::clone(&stderr_capture);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut output = String::new();
        let _ = reader.read_to_string(&mut output);
        if output.len() > 16 * 1024 {
            let keep_from = output.len() - 16 * 1024;
            output = output[keep_from..].to_string();
        }
        if let Ok(mut captured) = stderr_for_thread.lock() {
            *captured = output;
        }
    });

    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader
            .read_line(&mut line)
            .map(|read| (read, line, reader))
            .map_err(|error| error.to_string());
        let _ = sender.send(result);
    });
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(10_000));
    let (read, line, stdout) = match receiver.recv_timeout(timeout) {
        Ok(Ok((read, line, stdout))) => (read, line, stdout),
        Ok(Err(error)) => {
            terminate_child(&mut child);
            return Err(format!(
                "Failed to load extension: bridge read failed: {error}"
            ));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            terminate_child(&mut child);
            return Err(format!(
                "Failed to load extension: extension factory timed out for {extension_path}"
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            terminate_child(&mut child);
            return Err("Failed to load extension: bridge handshake disconnected".to_string());
        }
    };
    if read == 0 {
        let error = format_child_exit_after_parts(&mut child, &stderr_capture);
        return Err(format!("Failed to load extension: {error}"));
    }
    let ready: serde_json::Value = serde_json::from_str(line.trim()).map_err(|error| {
        terminate_child(&mut child);
        format!("Failed to load extension: bridge returned invalid JSON: {error}")
    })?;
    if ready.get("type") == Some(&serde_json::Value::String("load_error".to_string())) {
        let error = ready
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("extension factory failed");
        terminate_child(&mut child);
        return Err(format!("Failed to load extension: {error}"));
    }
    if ready.get("type") != Some(&serde_json::Value::String("ready".to_string())) {
        terminate_child(&mut child);
        return Err("Failed to load extension: bridge did not return a ready frame".to_string());
    }
    let process = Arc::new(ExternalExtensionProcess {
        state: Mutex::new(ExternalProcessState {
            child,
            stdin: BufWriter::new(stdin),
            stdout,
            stderr: stderr_capture,
        }),
        next_request_id: AtomicU64::new(1),
    });
    Ok((process, ready))
}

fn format_child_exit_after_parts(
    child: &mut std::process::Child,
    stderr: &Arc<Mutex<String>>,
) -> String {
    let status = child.try_wait().ok().flatten();
    match status {
        Some(status) => format_child_exit(status, stderr),
        None => "Extension bridge closed stdout unexpectedly".to_string(),
    }
}

fn required_string(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Extension bridge metadata missing string field {key:?}"))
}

fn registration_kind(value: &str) -> Option<RegistrationKind> {
    match value {
        "handler" => Some(RegistrationKind::Handler),
        "tool" => Some(RegistrationKind::Tool),
        "command" => Some(RegistrationKind::Command),
        "shortcut" => Some(RegistrationKind::Shortcut),
        "flag" => Some(RegistrationKind::Flag),
        "message_renderer" => Some(RegistrationKind::MessageRenderer),
        "markdown_transformer" => Some(RegistrationKind::MarkdownTransformer),
        "entry_renderer" => Some(RegistrationKind::EntryRenderer),
        _ => None,
    }
}

fn external_extension_from_metadata(
    extension_path: &str,
    resolved_path: &Path,
    metadata: &serde_json::Value,
    process: Arc<ExternalExtensionProcess>,
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> Result<Extension, String> {
    let mut extension = make_extension(extension_path, resolved_path);

    if let Some(registrations) = metadata
        .get("registrations")
        .and_then(serde_json::Value::as_array)
    {
        for registration in registrations {
            let Some(kind) = registration
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .and_then(registration_kind)
            else {
                return Err("Extension bridge returned an unknown registration kind".to_string());
            };
            let name = registration
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            extension.record_registration(kind, name);
        }
    }

    if let Some(handlers) = metadata
        .get("handlers")
        .and_then(serde_json::Value::as_array)
    {
        for handler in handlers {
            let event = required_string(handler, "event")?;
            let count = handler
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "Extension bridge handler metadata missing count".to_string())?;
            for index in 0..count as usize {
                extension
                    .handlers
                    .entry(event.clone())
                    .or_default()
                    .push(external_handler(Arc::clone(&process), event.clone(), index));
            }
        }
    }

    if let Some(commands) = metadata
        .get("commands")
        .and_then(serde_json::Value::as_array)
    {
        for command in commands {
            let name = required_string(command, "name")?;
            let description = command
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            extension.commands.insert(
                name.clone(),
                RegisteredCommand {
                    name: name.clone(),
                    source_info: extension.source_info.clone(),
                    description,
                    handler: external_command_handler(Arc::clone(&process), name),
                },
            );
        }
    }

    if let Some(tools) = metadata.get("tools").and_then(serde_json::Value::as_array) {
        for tool in tools {
            let name = required_string(tool, "name")?;
            extension.tools.insert(
                name.clone(),
                RegisteredTool {
                    name,
                    description: tool
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    parameters: tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                    source_info: extension.source_info.clone(),
                },
            );
        }
    }

    if let Some(flags) = metadata.get("flags").and_then(serde_json::Value::as_array) {
        for flag in flags {
            let name = required_string(flag, "name")?;
            let flag_type = match flag.get("type").and_then(serde_json::Value::as_str) {
                Some("boolean") => FlagType::Boolean,
                Some("string") => FlagType::String,
                Some(other) => return Err(format!("Unsupported extension flag type {other:?}")),
                None => return Err(format!("Extension flag {name:?} is missing type")),
            };
            let default = flag.get("default").cloned();
            if let Some(default) = &default {
                let valid = match flag_type {
                    FlagType::Boolean => default.is_boolean(),
                    FlagType::String => default.is_string(),
                };
                if !valid {
                    let expected = match flag_type {
                        FlagType::Boolean => "boolean",
                        FlagType::String => "string",
                    };
                    let actual = match default {
                        serde_json::Value::Null => "object",
                        serde_json::Value::Bool(_) => "boolean",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Array(_) | serde_json::Value::Object(_) => "object",
                    };
                    return Err(format!(
                        "Invalid default for flag \"{name}\": expected {expected}, got {actual}"
                    ));
                }
            }
            extension.flags.insert(
                name.clone(),
                ExtensionFlag {
                    name: name.clone(),
                    description: flag
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    flag_type,
                    default: default.clone(),
                    extension_path: extension_path.to_string(),
                },
            );
            if let Some(default) = default {
                if let Ok(mut guard) = runtime.lock() {
                    guard.flag_values.entry(name).or_insert(default);
                }
            }
        }
    }

    if let Some(shortcuts) = metadata
        .get("shortcuts")
        .and_then(serde_json::Value::as_array)
    {
        for shortcut in shortcuts {
            let name = required_string(shortcut, "shortcut")?;
            extension.shortcuts.insert(
                name.clone(),
                ExtensionShortcut {
                    shortcut: name.clone(),
                    description: shortcut
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    handler: external_shortcut_handler(Arc::clone(&process), name),
                    extension_path: extension_path.to_string(),
                },
            );
        }
    }

    if let Some(renderers) = metadata
        .get("messageRenderers")
        .and_then(serde_json::Value::as_array)
    {
        for renderer in renderers {
            let name = renderer
                .as_str()
                .ok_or_else(|| {
                    "Extension bridge message renderer name is not a string".to_string()
                })?
                .to_string();
            extension.message_renderers.insert(
                name.clone(),
                external_message_renderer(Arc::clone(&process), name),
            );
        }
    }
    if let Some(renderers) = metadata
        .get("entryRenderers")
        .and_then(serde_json::Value::as_array)
    {
        for renderer in renderers {
            let name = renderer
                .as_str()
                .ok_or_else(|| "Extension bridge entry renderer name is not a string".to_string())?
                .to_string();
            extension.entry_renderers.insert(
                name.clone(),
                external_entry_renderer(Arc::clone(&process), name),
            );
        }
    }
    if metadata
        .get("markdownTransformer")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        extension.markdown_transformer = Some(external_markdown_transformer(Arc::clone(&process)));
    }

    if let Some(providers) = metadata
        .get("providers")
        .and_then(serde_json::Value::as_array)
    {
        for provider in providers {
            queue_provider_registration(
                runtime,
                PendingProviderRegistration {
                    name: required_string(provider, "name")?,
                    config: provider
                        .get("config")
                        .cloned()
                        .ok_or_else(|| "Extension bridge provider is missing config".to_string())?,
                    extension_path: extension_path.to_string(),
                },
            );
        }
    }
    if metadata
        .get("nativeProviders")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|providers| !providers.is_empty())
    {
        return Err(
            "External extension bridge does not support native provider callbacks".to_string(),
        );
    }

    Ok(extension)
}

fn run_external_extension_with_runtime(
    extension_path: &str,
    resolved_path: &Path,
    runner: Option<&str>,
    timeout_ms: Option<u64>,
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> Result<Extension, String> {
    let runner = match runner {
        Some(runner) => runner.to_string(),
        None => {
            if command_on_path("node") {
                "node".to_string()
            } else if command_on_path("bun") {
                "bun".to_string()
            } else {
                return Err(
                    "Failed to load extension: no external extension runner found on PATH (expected node or bun)"
                        .to_string(),
                );
            }
        }
    };
    if !is_javascript_runtime(&runner) {
        return run_external_extension_legacy(extension_path, resolved_path, &runner, timeout_ms);
    }
    let (process, ready) =
        spawn_external_bridge(extension_path, resolved_path, &runner, timeout_ms)?;
    external_extension_from_metadata(extension_path, resolved_path, &ready, process, runtime)
}

fn resolve_external_runner(runner: Option<&str>) -> Result<String, String> {
    match runner {
        Some(runner) => Ok(runner.to_string()),
        None => {
            if command_on_path("node") {
                Ok("node".to_string())
            } else if command_on_path("bun") {
                Ok("bun".to_string())
            } else {
                Err(
                    "Failed to load extension: no external extension runner found on PATH (expected node or bun)"
                        .to_string(),
                )
            }
        }
    }
}

/// Spawn an external extension runner for a resolved entry.
///
/// Argument protocol: `node <entry>` (bun when node is absent). The entry is
/// spawned with the containing directory as cwd. Returns the extension record
/// on exit 0 (hidden=false) or a deterministic error on nonzero exit.
pub fn run_external_extension(
    extension_path: &str,
    resolved_path: &Path,
    runner: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<Extension, String> {
    let runner = resolve_external_runner(runner)?;
    run_external_extension_legacy(extension_path, resolved_path, &runner, timeout_ms)
}

fn run_external_extension_legacy(
    extension_path: &str,
    resolved_path: &Path,
    runner: &str,
    timeout_ms: Option<u64>,
) -> Result<Extension, String> {
    let cwd = resolved_path.parent().unwrap_or_else(|| Path::new("."));
    let mut command = Command::new(runner);
    command
        .arg(resolved_path)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to load extension: {e}"))?;
    if let Some(timeout_ms) = timeout_ms {
        let start = std::time::Instant::now();
        loop {
            if let Some(_status) = child
                .try_wait()
                .map_err(|e| format!("Failed to load extension: {e}"))?
            {
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("Failed to load extension: {e}"))?;
                return finish_child_output(output, extension_path)
                    .map(|_| make_extension(extension_path, resolved_path));
            }
            if start.elapsed().as_millis() >= timeout_ms as u128 {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Failed to load extension: extension runner timed out for {extension_path}"
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    } else {
        let output = child
            .wait_with_output()
            .map_err(|e| format!("Failed to load extension: {e}"))?;
        finish_child_output(output, extension_path)
            .map(|_| make_extension(extension_path, resolved_path))
    }
}

fn finish_child_output(output: std::process::Output, _extension_path: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr.trim();
    if summary.is_empty() {
        Err(format!(
            "Failed to load extension: runner exited with code {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ))
    } else {
        Err(format!("Failed to load extension: {summary}"))
    }
}

fn make_extension(extension_path: &str, resolved_path: &Path) -> Extension {
    let source = if extension_path.starts_with('<') && extension_path.ends_with('>') {
        extract_synthetic_source(extension_path)
    } else {
        "local".to_string()
    };
    let base_dir = if extension_path.starts_with('<') {
        None
    } else {
        resolved_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
    };
    Extension {
        path: extension_path.to_string(),
        resolved_path: resolved_path.to_string_lossy().into_owned(),
        hidden: false,
        source_info: SourceInfo::synthetic(extension_path, &source, base_dir),
        ..Default::default()
    }
}

/// Load a Rust-native extension factory with the same failure boundary as the
/// upstream in-process loader. Registration is atomic from the caller's
/// perspective: a factory error or panic returns an error and no extension is
/// added to the loaded set.
pub fn load_extension_from_factory<F>(
    factory: F,
    cwd: &str,
    runtime: Arc<Mutex<ExtensionRuntime>>,
    extension_path: &str,
) -> Result<Extension, ExtensionLoadError>
where
    F: FnOnce(&mut ExtensionApi<'_>) -> Result<(), String>,
{
    let resolved = resolve_relative_path(extension_path, cwd);
    let mut extension = make_extension(extension_path, &resolved);
    let factory_result = catch_unwind(AssertUnwindSafe(|| {
        let mut api = ExtensionApi::new(&mut extension, runtime, extension_path);
        factory(&mut api)
    }));
    match factory_result {
        Ok(Ok(())) => Ok(extension),
        Ok(Err(error)) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error: format!("Failed to load extension: {error}"),
        }),
        Err(payload) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error: format!(
                "Failed to load extension: extension factory panicked: {}",
                panic_message(payload)
            ),
        }),
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

/// Upstream source derivation: `<inline:name>` -> "inline"; `<bundled:name>`
/// -> "bundled". The source is the part before the colon inside the brackets.
fn extract_synthetic_source(extension_path: &str) -> String {
    let inner = &extension_path[1..extension_path.len() - 1];
    inner.split(':').next().unwrap_or("temporary").to_string()
}

fn command_on_path(name: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join(name).is_file() {
                return true;
            }
            #[cfg(windows)]
            if dir.join(format!("{name}.exe")).is_file() {
                return true;
            }
        }
    }
    false
}

/// Load one extension path: resolve relative to `cwd`, then run the external
/// runner. Returns the extension or a recorded error (upstream `loadExtension`).
pub fn load_extension(
    extension_path: &str,
    cwd: &str,
    runner: Option<&str>,
) -> Result<Extension, ExtensionLoadError> {
    let runtime = create_extension_runtime();
    load_extension_with_runtime(extension_path, cwd, runner, &runtime)
}

fn load_extension_with_runtime(
    extension_path: &str,
    cwd: &str,
    runner: Option<&str>,
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> Result<Extension, ExtensionLoadError> {
    let resolved = resolve_relative_path(extension_path, cwd);
    match run_external_extension_with_runtime(extension_path, &resolved, runner, None, runtime) {
        Ok(extension) => Ok(extension),
        Err(error) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error,
        }),
    }
}

/// Resolve a path against a base directory without canonicalization
/// (a conservative stand-in for the upstream `resolvePath`).
pub fn resolve_relative_path(path: &str, base: &str) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        path_buf
    } else {
        Path::new(base).join(path_buf)
    }
}

/// Create the shared extension runtime (upstream `createExtensionRuntime`).
pub fn create_extension_runtime() -> Arc<Mutex<ExtensionRuntime>> {
    Arc::new(Mutex::new(ExtensionRuntime::new()))
}

/// Load extensions from explicit paths (upstream `loadExtensions`).
pub fn load_extensions(
    paths: &[String],
    cwd: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
) -> LoadExtensionsResult {
    let runtime = runtime.unwrap_or_else(create_extension_runtime);
    let mut extensions = Vec::new();
    let mut errors = Vec::new();
    for ext_path in paths {
        match load_extension_with_runtime(ext_path, cwd, runner, &runtime) {
            Ok(extension) => extensions.push(extension),
            Err(error) => errors.push(error),
        }
    }
    apply_flag_defaults(&runtime, &extensions);
    LoadExtensionsResult {
        extensions,
        errors,
        runtime,
    }
}

/// Discover and load extensions from standard locations (upstream
/// `discoverAndLoadExtensions`):
/// 1. project-local `cwd/.pi/extensions/`
/// 2. global `agentDir/extensions/`
/// 3. explicitly configured paths
pub fn discover_and_load_extensions(
    configured_paths: &[String],
    cwd: &str,
    agent_dir: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
) -> LoadExtensionsResult {
    let mut all_paths: Vec<String> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut add_paths = |paths: Vec<PathBuf>| {
        for p in paths {
            let absolute = if p.is_absolute() {
                p.clone()
            } else {
                Path::new(cwd).join(&p)
            };
            if seen.insert(absolute.clone()) {
                all_paths.push(absolute.to_string_lossy().into_owned());
            }
        }
    };

    // 1. Project-local extensions.
    let local_ext_dir = Path::new(cwd).join(CONFIG_DIR_NAME).join("extensions");
    add_paths(discover_extensions_in_dir(&local_ext_dir));

    // 2. Global extensions.
    let global_ext_dir = Path::new(agent_dir).join("extensions");
    add_paths(discover_extensions_in_dir(&global_ext_dir));

    // 3. Explicitly configured paths.
    for p in configured_paths {
        let resolved = resolve_relative_path(p, cwd);
        if resolved.is_dir() {
            if let Some(entries) = resolve_extension_entries(&resolved) {
                add_paths(entries);
                continue;
            }
            add_paths(discover_extensions_in_dir(&resolved));
            continue;
        }
        add_paths(vec![resolved]);
    }

    load_extensions(&all_paths, cwd, runtime, runner)
}

/// Load an inline/bundled extension by spawning its entry as a subprocess
/// (upstream `loadExtensionFromFactory`: the wrapper transforms a bundled
/// extension into a runnable subprocess; in the JS runtime it executes the
/// factory in-process, and the port executes the entry file with the external
/// runner instead).
pub fn load_bundled_extension(
    extension_path: &str,
    runner: Option<&str>,
) -> Result<Extension, ExtensionLoadError> {
    let path = PathBuf::from(extension_path);
    let resolved = if path.is_absolute() {
        path
    } else {
        Path::new(".").join(&path)
    };
    let runtime = create_extension_runtime();
    match run_external_extension_with_runtime(extension_path, &resolved, runner, None, &runtime) {
        Ok(extension) => Ok(extension),
        Err(error) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error,
        }),
    }
}

/// Runtime helpers used by runner.rs: queue a provider registration.
pub fn queue_provider_registration(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    registration: PendingProviderRegistration,
) {
    if let Ok(mut guard) = runtime.lock() {
        if guard.assert_active().is_ok() {
            guard.pending_provider_registrations.push(registration);
        }
    }
}

pub fn queue_native_provider_registration(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    registration: PendingNativeProviderRegistration,
) {
    if let Ok(mut guard) = runtime.lock() {
        if guard.assert_active().is_ok() {
            guard
                .pending_native_provider_registrations
                .push(registration);
        }
    }
}

/// Serialize the runtime's queued provider registrations (upstream flush).
pub fn take_pending_provider_registrations(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> (
    Vec<PendingProviderRegistration>,
    Vec<PendingNativeProviderRegistration>,
) {
    let Ok(mut guard) = runtime.lock() else {
        return (Vec::new(), Vec::new());
    };
    (
        std::mem::take(&mut guard.pending_provider_registrations),
        std::mem::take(&mut guard.pending_native_provider_registrations),
    )
}

/// Build the `VirtualModule`-style flag map from registered extension flags:
/// defaults are applied when no CLI value exists (upstream `registerFlag`).
pub fn apply_flag_defaults(runtime: &Arc<Mutex<ExtensionRuntime>>, extensions: &[Extension]) {
    let Ok(mut guard) = runtime.lock() else {
        return;
    };
    for extension in extensions {
        for (name, flag) in &extension.flags {
            if let Some(default) = &flag.default {
                if !guard.flag_values.contains_key(name) {
                    guard.flag_values.insert(name.clone(), default.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sandbox(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pi-ext-loader-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extension_file_detection() {
        assert!(is_extension_file("a.ts"));
        assert!(is_extension_file("b.js"));
        assert!(!is_extension_file("c.json"));
        assert!(!is_extension_file("d.md"));
    }

    #[test]
    fn resolve_entries_prefers_pi_manifest() {
        let dir = sandbox("manifest");
        fs::write(
            dir.join("package.json"),
            r#"{ "pi": { "extensions": ["main.ts"] } }"#,
        )
        .unwrap();
        fs::write(dir.join("main.ts"), "export default () => {}").unwrap();
        fs::write(dir.join("index.ts"), "export default () => {}").unwrap();
        let entries = resolve_extension_entries(&dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().unwrap().to_string_lossy(), "main.ts");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entries_falls_back_to_index() {
        let dir = sandbox("index");
        fs::write(dir.join("index.js"), "module.exports = () => {}").unwrap();
        let entries = resolve_extension_entries(&dir).unwrap();
        assert_eq!(
            entries[0].file_name().unwrap().to_string_lossy(),
            "index.js"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entries_none_when_no_entry() {
        let dir = sandbox("none");
        assert!(resolve_extension_entries(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_direct_files_and_subdirs() {
        let dir = sandbox("discover");
        fs::write(dir.join("one.ts"), "x").unwrap();
        fs::write(dir.join("two.js"), "x").unwrap();
        fs::write(dir.join("three.md"), "x").unwrap();
        fs::create_dir_all(dir.join("pkg")).unwrap();
        fs::write(
            dir.join("pkg/package.json"),
            r#"{ "pi": { "extensions": ["ext.ts"] } }"#,
        )
        .unwrap();
        fs::write(dir.join("pkg/ext.ts"), "x").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/index.ts"), "x").unwrap();
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::create_dir_all(dir.join("nested/deep")).unwrap();
        fs::write(dir.join("nested/deep/index.js"), "x").unwrap(); // only one level
        let discovered = discover_extensions_in_dir(&dir);
        let names: Vec<String> = discovered
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"one.ts".to_string()), "{names:?}");
        assert!(names.contains(&"two.js".to_string()), "{names:?}");
        assert!(names.contains(&"pkg/ext.ts".to_string()), "{names:?}");
        assert!(names.contains(&"sub/index.ts".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("nested/")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("three.md")), "{names:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_fails_when_no_runner() {
        // No node/bun guaranteed on PATH; with a nonexistent runner the error
        // must be deterministic.
        let dir = sandbox("norunner");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let result = run_external_extension("index.ts", &entry, Some("/nonexistent/runner"), None);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_records_error_for_missing_runner_default() {
        // When neither node nor bun is on PATH, the default runner resolution
        // fails with the upstream-style "Failed to load extension:" prefix.
        if command_on_path("node") || command_on_path("bun") {
            return; // environment has a runner; covered by fake-runner tests
        }
        let dir = sandbox("nort");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let err = load_extension("index.ts", &dir.to_string_lossy(), None).unwrap_err();
        let _ = &err;
        assert!(
            err.error.starts_with("Failed to load extension:"),
            "{}",
            err.error
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn execute_extension_with_fake_node() {
        // Fake `node`: a script that records its argv and exits 0.
        let dir = sandbox("fake");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let bin = sandbox("bin");
        let node_path = bin.join("node");
        let log_path = bin.join("log");
        let script = format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > \"{}\"\n",
            log_path.display()
        );
        fs::write(&node_path, &script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&node_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result =
            run_external_extension("index.ts", &entry, Some(node_path.to_str().unwrap()), None);
        assert!(result.is_ok(), "result was an error: {result:?}");
        let extension = result.unwrap();
        assert_eq!(extension.path, "index.ts");
        // The arg protocol is `node <resolved entry>`.
        let logged = fs::read_to_string(&log_path).unwrap();
        assert_eq!(logged, entry.to_string_lossy());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn execute_extension_fake_node_failure_reports_error() {
        let dir = sandbox("fakefail");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let bin = sandbox("binfail");
        let node_path = bin.join("node");
        fs::write(
            &node_path,
            "#!/bin/sh\necho 'boom: bad extension' >&2\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&node_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let err =
            run_external_extension("index.ts", &entry, Some(node_path.to_str().unwrap()), None)
                .unwrap_err();
        assert!(err.starts_with("Failed to load extension:"), "{err}");
        assert!(err.contains("boom: bad extension"), "{err}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn discover_and_load_collects_project_and_global() {
        let root = sandbox("disc");
        let cwd = root.join("proj");
        let agent_dir = root.join("agent");
        fs::create_dir_all(cwd.join(".pi/extensions")).unwrap();
        fs::create_dir_all(agent_dir.join("extensions")).unwrap();
        fs::write(cwd.join(".pi/extensions/local.ts"), "x").unwrap();
        fs::write(agent_dir.join("extensions/global.ts"), "x").unwrap();
        let result = discover_and_load_extensions(
            &[],
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            None,
            None,
        );
        // Both discovered; loads may record errors if no runner exists, but
        // the discovery surfaces both entries either as extensions or errors.
        let count = result.extensions.len() + result.errors.len();
        assert_eq!(
            count,
            2,
            "expected 2 entries; got {} extensions and {} errors",
            result.extensions.len(),
            result.errors.len()
        );
        let paths: Vec<String> = result
            .extensions
            .iter()
            .map(|e| e.path.clone())
            .chain(result.errors.iter().map(|e| e.path.clone()))
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("local.ts")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("global.ts")), "{paths:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_paths_are_included_and_deduped() {
        let root = sandbox("cfg");
        let cwd = root.join("proj");
        let agent_dir = root.join("agent");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&agent_dir).unwrap();
        let ext = cwd.join("ext.ts");
        fs::write(&ext, "x").unwrap();
        // The configured path resolves to the same project-local extension.
        let configured = vec![ext.to_string_lossy().to_string()];
        let result = discover_and_load_extensions(
            &configured,
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            None,
            None,
        );
        let paths: Vec<String> = result
            .extensions
            .iter()
            .map(|e| e.path.clone())
            .chain(result.errors.iter().map(|e| e.path.clone()))
            .collect();
        // One occurrence total (deduped).
        assert_eq!(paths.len(), 1, "{paths:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn synthetic_source_extraction() {
        assert_eq!(extract_synthetic_source("<inline:foo>"), "inline");
        assert_eq!(extract_synthetic_source("<bundled:llama.cpp>"), "bundled");
    }
}
