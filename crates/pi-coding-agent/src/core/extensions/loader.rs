//! Extension loader — port of
//! `packages/coding-agent/src/core/extensions/loader.ts`.
//!
//! Rust cannot execute TypeScript extension modules in-process (the upstream
//! uses jiti imports). The port keeps the exact discovery/resolution surface
//! and uses a persistent Node/Bun JSON-lines bridge for the supported external
//! runtime boundary. The bridge awaits the factory, returns registration
//! metadata, and keeps the JavaScript callbacks alive for command, hook, and
//! renderer/tool calls, native provider callbacks, and bidirectional
//! host-action frames. It deliberately does not claim to embed jiti or
//! virtual modules; those remain explicit runtime-boundary limitations.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::CONFIG_DIR_NAME;
use crate::core::extensions::types::{
    EntryRenderer, Extension, ExtensionFlag, ExtensionHostAction, ExtensionLoadError,
    ExtensionRuntime, ExtensionShortcut, FlagType, HandlerFn, LoadExtensionsResult,
    MarkdownTransformer, MessageRenderer, NativeProviderCallbackFn,
    PendingNativeProviderRegistration, PendingProviderRegistration, RegisteredCommand,
    RegisteredTool, RegistrationKind, SourceInfo, ToolExecuteFn, ToolExecutionRequest,
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
    /// factories. External bridge callbacks are attached when the bridge
    /// metadata is materialized.
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
                definition: serde_json::json!({"id": provider}),
                callbacks: std::collections::BTreeMap::new(),
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

/// Bootstrap executed by a real Node/Bun runner. Stdout carries JSONL request,
/// response, and host-action frames, while extension logs are redirected to
/// stderr so they cannot corrupt the protocol stream.
const EXTERNAL_EXTENSION_BRIDGE: &str = r###"
import { pathToFileURL } from "node:url";
import * as readline from "node:readline";

const entryPath = process.argv.at(-1);
const NOT_INITIALIZED = "Extension runtime not initialized. Action methods cannot be called during extension loading.";
const protocolStdoutWrite = process.stdout.write.bind(process.stdout);
// Extension code is allowed to log, but stdout belongs exclusively to the
// JSONL protocol. Keep the original writer for send() and divert all direct
// extension writes to stderr as well.
process.stdout.write = (chunk, encoding, callback) => process.stderr.write(chunk, encoding, callback);
const state = {
  active: true,
  loading: true,
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
  nextHostActionId: 1,
  pendingHostActions: new Map(),
  syncHost: {
    bound: false,
    sessionName: null,
    activeTools: [],
    allTools: [],
    commands: [],
    thinkingLevel: "medium",
    model: undefined,
    scopedModels: [],
    isIdle: true,
    isProjectTrusted: true,
    signal: undefined,
    hasPendingMessages: false,
    contextUsage: undefined,
    systemPrompt: "",
    systemPromptOptions: {},
  },
  toolSignalAborted: false,
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

function hostAction(action, args = {}) {
  assertActive();
  if (state.loading) throw new Error(NOT_INITIALIZED);
  const id = `host-${state.nextHostActionId++}`;
  const promise = new Promise((resolve, reject) => {
  state.pendingHostActions.set(id, { resolve, reject, action });
  });
  // Preserve the upstream fire-and-forget shape for callers that do not await
  // a void action, without turning a rejected host callback into an
  // unhandled-rejection process failure.
  promise.catch(() => {});
  send({ type: "host_action", id, action, args });
  return promise;
}

function fireHostAction(action, args = {}) {
  void hostAction(action, args);
}

function assertSyncHostReady() {
  assertActive();
  if (state.loading || !state.syncHost.bound) throw new Error(NOT_INITIALIZED);
}

function syncHostSnapshot(snapshot, bound) {
  const value = snapshot && typeof snapshot === "object" ? snapshot : {};
  state.syncHost = {
    bound: Boolean(bound),
    sessionName: value.sessionName ?? null,
    activeTools: Array.isArray(value.activeTools) ? value.activeTools : [],
    allTools: Array.isArray(value.allTools) ? value.allTools : [],
    commands: Array.isArray(value.commands) ? value.commands : [],
    thinkingLevel: typeof value.thinkingLevel === "string" ? value.thinkingLevel : "medium",
    model: value.model ?? undefined,
    scopedModels: Array.isArray(value.scopedModels) ? value.scopedModels : [],
    isIdle: typeof value.isIdle === "boolean" ? value.isIdle : true,
    isProjectTrusted: typeof value.isProjectTrusted === "boolean" ? value.isProjectTrusted : true,
    signal: value.signal && typeof value.signal === "object"
      ? { aborted: Boolean(value.signal.aborted) }
      : undefined,
    hasPendingMessages: Boolean(value.hasPendingMessages),
    contextUsage: value.contextUsage ?? undefined,
    systemPrompt: typeof value.systemPrompt === "string" ? value.systemPrompt : "",
    systemPromptOptions: value.systemPromptOptions && typeof value.systemPromptOptions === "object"
      ? value.systemPromptOptions
      : {},
  };
  state.toolSignalAborted = Boolean(state.syncHost.signal?.aborted);
}

function resolveHostAction(message) {
  const pending = state.pendingHostActions.get(String(message.id));
  if (!pending) return;
  state.pendingHostActions.delete(String(message.id));
  if (pending.action === "abort" && message.ok !== false) state.toolSignalAborted = true;
  if (message.ok === false) {
    pending.reject(new Error(message.error ?? "Extension host action failed"));
  } else {
    pending.resolve(message.result ?? null);
  }
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
      execute: tool.execute,
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
    if (typeof nameOrProvider === "string") {
      if (config === undefined) throw new Error("Provider config is required when registering by name");
      if (hasFunction(config)) {
        throw new Error("External extension bridge only supports JSON provider configs");
      }
      state.providers.push({ name: nameOrProvider, config });
      return;
    }
    if (!nameOrProvider || typeof nameOrProvider !== "object") {
      throw new Error("Native provider registration requires a provider object");
    }
    if (typeof nameOrProvider.id !== "string" || nameOrProvider.id.length === 0) {
      throw new Error("Native provider registration requires a provider id");
    }
    const callbacks = new Map();
    for (const [name, callback] of Object.entries(nameOrProvider)) {
      if (name !== "id" && typeof callback === "function") callbacks.set(name, callback);
    }
    if (callbacks.size === 0) {
      throw new Error(`Native provider ${nameOrProvider.id} must define at least one callback`);
    }
    const definition = Object.fromEntries(
      Object.entries(nameOrProvider).filter(([, value]) => typeof value !== "function"),
    );
    state.nativeProviders.push({ name: nameOrProvider.id, callbacks, definition });
  },
  unregisterProvider(name) {
    assertActive();
    state.providers = state.providers.filter((registration) => registration.name !== name);
    state.nativeProviders = state.nativeProviders.filter((registration) => registration.name !== name);
  },
  sendMessage(message, options) {
    fireHostAction("sendMessage", { message, options: options ?? null });
  },
  sendUserMessage(content, options) {
    fireHostAction("sendUserMessage", { content, options: options ?? null });
  },
  appendEntry(customType, data) {
    fireHostAction("appendEntry", { customType, data: data ?? null });
  },
  setSessionName(name) {
    assertSyncHostReady();
    state.syncHost.sessionName = name;
    fireHostAction("setSessionName", { name });
  },
  getSessionName() {
    assertSyncHostReady();
    return state.syncHost.sessionName ?? undefined;
  },
  setLabel(entryId, label) {
    fireHostAction("setLabel", { entryId, label: label ?? null });
  },
  getActiveTools() {
    assertSyncHostReady();
    return [...state.syncHost.activeTools];
  },
  getAllTools() {
    assertSyncHostReady();
    return state.syncHost.allTools;
  },
  setActiveTools(toolNames) {
    assertSyncHostReady();
    state.syncHost.activeTools = Array.isArray(toolNames) ? [...toolNames] : [];
    fireHostAction("setActiveTools", { toolNames });
  },
  getCommands() {
    assertSyncHostReady();
    return state.syncHost.commands;
  },
  setModel(model) {
    return hostAction("setModel", { model });
  },
  getThinkingLevel() {
    assertSyncHostReady();
    return state.syncHost.thinkingLevel;
  },
  setThinkingLevel(level) {
    assertSyncHostReady();
    state.syncHost.thinkingLevel = level;
    fireHostAction("setThinkingLevel", { level });
  },
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
    nativeProviders: state.nativeProviders.map(({ name, callbacks, definition }) => ({
      name,
      callbacks: [...callbacks.keys()],
      definition,
    })),
    registrations: state.registrations,
  };
}

function createSignalLike() {
  const signal = {};
  Object.defineProperty(signal, "aborted", {
    configurable: false,
    enumerable: true,
    get: () => state.toolSignalAborted,
  });
  signal.throwIfAborted = () => {
    if (signal.aborted) throw new Error("Operation aborted");
  };
  return signal;
}

function contextFor(request) {
  syncHostSnapshot(request.hostState, request.hostBound);
  const context = { ...(request.context ?? {}) };
  // Extension tool callbacks receive the same host-bound action surface as
  // the extension API. Keep void actions fire-and-forget, while getters read
  // the per-request snapshot synchronously and setModel remains awaitable.
  context.model = state.syncHost.model;
  context.scopedModels = [...state.syncHost.scopedModels];
  context.isIdle = () => {
    assertSyncHostReady();
    return state.syncHost.isIdle;
  };
  context.isProjectTrusted = () => {
    assertSyncHostReady();
    return state.syncHost.isProjectTrusted;
  };
  context.signal = state.syncHost.signal ? createSignalLike() : undefined;
  context.abort = () => {
    assertSyncHostReady();
    state.toolSignalAborted = true;
    fireHostAction("abort", { toolCallId: request.toolCallId ?? null });
  };
  context.hasPendingMessages = () => {
    assertSyncHostReady();
    return state.syncHost.hasPendingMessages;
  };
  context.shutdown = () => {
    assertSyncHostReady();
    fireHostAction("shutdown", { toolCallId: request.toolCallId ?? null });
  };
  context.getContextUsage = () => {
    assertSyncHostReady();
    return state.syncHost.contextUsage;
  };
  context.compact = (options) => {
    assertSyncHostReady();
    fireHostAction("compact", {
      options: options ?? null,
      toolCallId: request.toolCallId ?? null,
    });
  };
  context.getSystemPrompt = () => {
    assertSyncHostReady();
    return typeof request.event?.systemPrompt === "string"
      ? request.event.systemPrompt
      : state.syncHost.systemPrompt;
  };
  context.getSystemPromptOptions = () => {
    assertSyncHostReady();
    return request.event?.systemPromptOptions ?? state.syncHost.systemPromptOptions;
  };
  context.sendMessage = (message, options) => fireHostAction("sendMessage", { message, options: options ?? null });
  context.sendUserMessage = (content, options) => fireHostAction("sendUserMessage", { content, options: options ?? null });
  context.appendEntry = (customType, data) => fireHostAction("appendEntry", { customType, data: data ?? null });
  context.setLabel = (entryId, label) => fireHostAction("setLabel", { entryId, label: label ?? null });
  context.setSessionName = (name) => {
    assertSyncHostReady();
    state.syncHost.sessionName = name;
    fireHostAction("setSessionName", { name });
  };
  context.getSessionName = () => {
    assertSyncHostReady();
    return state.syncHost.sessionName ?? undefined;
  };
  context.getActiveTools = () => {
    assertSyncHostReady();
    return [...state.syncHost.activeTools];
  };
  context.getAllTools = () => {
    assertSyncHostReady();
    return state.syncHost.allTools;
  };
  context.setActiveTools = (toolNames) => {
    assertSyncHostReady();
    state.syncHost.activeTools = Array.isArray(toolNames) ? [...toolNames] : [];
    fireHostAction("setActiveTools", { toolNames });
  };
  context.getCommands = () => {
    assertSyncHostReady();
    return state.syncHost.commands;
  };
  context.setModel = (model) => hostAction("setModel", { model });
  context.getThinkingLevel = () => {
    assertSyncHostReady();
    return state.syncHost.thinkingLevel;
  };
  context.setThinkingLevel = (level) => {
    assertSyncHostReady();
    state.syncHost.thinkingLevel = level;
    fireHostAction("setThinkingLevel", { level });
  };
  Object.defineProperty(context, "thinkingLevel", {
    configurable: true,
    enumerable: true,
    get: () => {
      assertSyncHostReady();
      return state.syncHost.thinkingLevel;
    },
  });
  return context;
}

async function collectNativeProviderEvents(result) {
  const stream = await result;
  if (Array.isArray(stream)) return stream;
  if (stream && typeof stream[Symbol.asyncIterator] === "function") {
    const events = [];
    for await (const event of stream) events.push(event);
    return events;
  }
  if (stream && typeof stream[Symbol.iterator] === "function") return [...stream];
  throw new Error("Native provider callback must return an async iterable, iterable, or array");
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
  if (request.kind === "tool") {
    const tool = state.tools.get(request.name);
    if (!tool) throw new Error(`Extension tool not found: ${request.name}`);
    const signal = createSignalLike();
    const onUpdate = (partialResult) => {
      fireHostAction("toolUpdate", {
        toolCallId: request.toolCallId ?? "",
        result: partialResult ?? null,
      });
    };
    return await tool.execute(
      request.toolCallId ?? "",
      request.params ?? {},
      signal,
      onUpdate,
      context,
    );
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
  if (request.kind === "native_provider") {
    const provider = state.nativeProviders.find((registration) => registration.name === request.name);
    if (!provider) throw new Error(`Native provider not found: ${request.name}`);
    const callback = provider.callbacks.get(request.callback);
    if (!callback) {
      throw new Error(`Native provider callback not found: ${request.name}.${request.callback}`);
    }
    return await collectNativeProviderEvents(
      callback(request.model ?? null, request.context ?? null, request.options ?? null),
    );
  }
  throw new Error(`Unknown extension bridge call: ${request.kind}`);
}

function send(message) {
  protocolStdoutWrite(`${JSON.stringify(message)}\n`);
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
  state.loading = false;
  send({ type: "ready", ...metadata() });

  const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  async function handleRequest(request) {
    try {
      const result = await invoke(request);
      send({ id: request.id, ok: true, result: result === undefined ? null : result });
    } catch (error) {
      send({ id: request?.id ?? null, ok: false, error: errorMessage(error) });
    }
  }
  // Host responses must be consumed while a callback is awaiting a Rust
  // action. Dispatching calls without awaiting here keeps the stdin reader
  // live; Rust serializes outbound requests, so callback order is preserved.
  for await (const line of input) {
    if (!line.trim()) continue;
    let request;
    try {
      request = JSON.parse(line);
      if (request.type === "host_response") {
        resolveHostAction(request);
      } else if (request.type === "close") {
        state.active = false;
        for (const pending of state.pendingHostActions.values()) {
          pending.reject(new Error("Extension bridge closed"));
        }
        state.pendingHostActions.clear();
        break;
      } else {
        void handleRequest(request);
      }
    } catch (error) {
      send({ id: request?.id ?? null, ok: false, error: errorMessage(error) });
    }
  }
}

main().catch((error) => {
  const message = errorMessage(error);
  const virtualSpecifier = [
    "typebox",
    "typebox/compile",
    "typebox/value",
    "@sinclair/typebox",
    "@sinclair/typebox/compile",
    "@sinclair/typebox/value",
    "@earendil-works/pi-agent-core",
    "@earendil-works/pi-tui",
    "@earendil-works/pi-ai",
    "@earendil-works/pi-ai/compat",
    "@earendil-works/pi-ai/oauth",
    "@earendil-works/pi-ai/providers/all",
    "@earendil-works/pi-coding-agent",
    "@mariozechner/pi-agent-core",
    "@mariozechner/pi-tui",
    "@mariozechner/pi-ai",
    "@mariozechner/pi-ai/compat",
    "@mariozechner/pi-ai/oauth",
    "@mariozechner/pi-ai/providers/all",
    "@mariozechner/pi-coding-agent",
  ].find((specifier) => message.includes(`'${specifier}'`) || message.includes(`\"${specifier}\"`));
  const diagnostic = virtualSpecifier
    ? `Virtual module \"${virtualSpecifier}\" is not resolvable by the pi-rust extension bridge. Install a runtime package that provides it or use a bundled runtime; pi-rust does not embed jiti virtual modules.`
    : typeof entryPath === "string" && entryPath.endsWith(".tsx")
      ? `TSX extension \"${entryPath}\" requires Bun or an explicit TypeScript/JSX transpiler; the Node bridge supports native TypeScript type stripping but does not embed jiti.`
      : message;
  send({ type: "load_error", error: diagnostic });
  process.exitCode = 1;
});
"###;

enum BridgeFrame {
    Line(String),
    Eof,
    Error(String),
}

const MAX_BRIDGE_FRAME_BYTES: usize = 8 * 1024 * 1024;

struct ExternalProcessState {
    child: std::process::Child,
    stdin: BufWriter<std::process::ChildStdin>,
    frames: mpsc::Receiver<BridgeFrame>,
    stderr: Arc<Mutex<String>>,
    stderr_done: mpsc::Receiver<()>,
    request_timeout: Duration,
}

thread_local! {
    static ACTIVE_BRIDGE_REQUESTS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

struct BridgeRequestGuard {
    process_id: u64,
}

impl BridgeRequestGuard {
    fn enter(process_id: u64) -> Result<Self, String> {
        let reentrant = ACTIVE_BRIDGE_REQUESTS.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&process_id) {
                true
            } else {
                active.push(process_id);
                false
            }
        });
        if reentrant {
            Err("Extension bridge callback re-entry rejected to avoid deadlock".to_string())
        } else {
            Ok(Self { process_id })
        }
    }
}

impl Drop for BridgeRequestGuard {
    fn drop(&mut self) {
        ACTIVE_BRIDGE_REQUESTS.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(position) = active.iter().rposition(|id| *id == self.process_id) {
                active.remove(position);
            }
        });
    }
}

struct ExternalExtensionProcess {
    state: Mutex<ExternalProcessState>,
    request_lock: Mutex<()>,
    next_request_id: AtomicU64,
    process_id: u64,
    closed: AtomicBool,
    runtime: Arc<Mutex<ExtensionRuntime>>,
}

static NEXT_EXTERNAL_PROCESS_ID: AtomicU64 = AtomicU64::new(1);

fn write_bridge_frame(
    stdin: &mut BufWriter<std::process::ChildStdin>,
    frame: &serde_json::Value,
) -> Result<(), String> {
    serde_json::to_writer(&mut *stdin, frame)
        .map_err(|error| format!("Extension bridge write failed: {error}"))?;
    stdin
        .write_all(b"\n")
        .map_err(|error| format!("Extension bridge write failed: {error}"))?;
    stdin
        .flush()
        .map_err(|error| format!("Extension bridge flush failed: {error}"))
}

fn dispatch_bridge_host_action(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    frame: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let action_name = frame
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Extension bridge host action is missing action name".to_string())?;
    let action = ExtensionHostAction::from_protocol_name(action_name)
        .ok_or_else(|| format!("Unknown extension host action: {action_name}"))?;
    let args = frame
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let actions = {
        let runtime = runtime
            .lock()
            .map_err(|_| "Extension runtime lock poisoned".to_string())?;
        runtime.host_action_handler()?
    };
    match catch_unwind(AssertUnwindSafe(|| actions.dispatch(action, &args))) {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "Extension host action panicked: {}",
            panic_message(payload)
        )),
    }
}

fn bridge_host_action_response(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    frame: &serde_json::Value,
) -> serde_json::Value {
    let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
    match dispatch_bridge_host_action(runtime, frame) {
        Ok(result) => serde_json::json!({
            "type": "host_response",
            "id": id,
            "ok": true,
            "result": result,
        }),
        Err(error) => serde_json::json!({
            "type": "host_response",
            "id": id,
            "ok": false,
            "error": error,
        }),
    }
}

fn respond_to_bridge_host_action(
    runtime: &Arc<Mutex<ExtensionRuntime>>,
    stdin: &mut BufWriter<std::process::ChildStdin>,
    frame: &serde_json::Value,
) -> Result<(), String> {
    if frame.get("id").is_none() {
        return Err("Extension bridge host action is missing request id".to_string());
    }
    write_bridge_frame(stdin, &bridge_host_action_response(runtime, frame))
}

impl ExternalExtensionProcess {
    fn request(&self, mut request: serde_json::Value) -> Result<Option<serde_json::Value>, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("Extension bridge is stale after runtime invalidation".to_string());
        }
        {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| "Extension runtime lock poisoned".to_string())?;
            runtime.assert_active()?;
        }
        let _request_guard = BridgeRequestGuard::enter(self.process_id)?;
        let _request_lock = self
            .request_lock
            .lock()
            .map_err(|_| "Extension bridge request lock poisoned".to_string())?;
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        request
            .as_object_mut()
            .ok_or_else(|| "Extension bridge request must be an object".to_string())?
            .insert("id".to_string(), serde_json::Value::from(id));
        let (host_state, host_bound) = {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| "Extension runtime lock poisoned".to_string())?;
            (
                runtime.host_action_snapshot_for(&request)?,
                runtime.has_host_actions(),
            )
        };
        let request_object = request
            .as_object_mut()
            .expect("request object checked above");
        request_object.insert("hostState".to_string(), host_state);
        request_object.insert("hostBound".to_string(), serde_json::Value::Bool(host_bound));
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Extension bridge lock poisoned".to_string())?;
        if let Some(status) = state
            .child
            .try_wait()
            .map_err(|error| format!("Extension bridge status failed: {error}"))?
        {
            return Err(format_child_exit(status, &state.stderr, &state.stderr_done));
        }
        write_bridge_frame(&mut state.stdin, &request)?;

        let deadline = Instant::now() + state.request_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait_for = remaining.min(Duration::from_millis(50));
            let frame = match state.frames.recv_timeout(wait_for) {
                Ok(frame) => frame,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.closed.load(Ordering::Acquire) {
                        terminate_child(&mut state.child);
                        return Err(
                            "Extension bridge is stale after runtime invalidation".to_string()
                        );
                    }
                    if Instant::now() < deadline {
                        continue;
                    }
                    terminate_child(&mut state.child);
                    return Err(format!(
                        "Extension bridge request timed out after {}ms",
                        state.request_timeout.as_millis()
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format_child_exit_after_eof(&mut state));
                }
            };
            let BridgeFrame::Line(line) = frame else {
                return match frame {
                    BridgeFrame::Eof => Err(format_child_exit_after_eof(&mut state)),
                    BridgeFrame::Error(error) => {
                        Err(format!("Extension bridge read failed: {error}"))
                    }
                    BridgeFrame::Line(_) => unreachable!(),
                };
            };
            if line.trim().is_empty() {
                continue;
            }
            let response: serde_json::Value = serde_json::from_str(line.trim())
                .map_err(|error| format!("Extension bridge returned invalid JSON: {error}"))?;
            if response.get("type") == Some(&serde_json::Value::String("host_action".into())) {
                // The request lock serializes protocol calls, but the process
                // state mutex is not held while arbitrary host code runs.
                drop(state);
                let host_response = bridge_host_action_response(&self.runtime, &response);
                state = self
                    .state
                    .lock()
                    .map_err(|_| "Extension bridge lock poisoned".to_string())?;
                write_bridge_frame(&mut state.stdin, &host_response)?;
                continue;
            }
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
            if self.closed.load(Ordering::Acquire) {
                return Err("Extension bridge is stale after runtime invalidation".to_string());
            }
            return Ok((!result.is_null()).then_some(result));
        }
    }

    fn request_native_provider_events(
        &self,
        provider: &str,
        callback: &str,
        model: serde_json::Value,
        context: serde_json::Value,
        options: serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, String> {
        let result = self
            .request(serde_json::json!({
                "type": "call",
                "kind": "native_provider",
                "name": provider,
                "callback": callback,
                "model": model,
                "context": context,
                "options": options,
            }))?
            .ok_or_else(|| "Native provider callback returned no event sequence".to_string())?;
        result.as_array().cloned().ok_or_else(|| {
            "Native provider callback returned a non-array event sequence".to_string()
        })
    }

    fn close_now(&self) {
        if let Ok(mut state) = self.state.lock() {
            let _ = state.stdin.write_all(b"{\"type\":\"close\"}\n");
            let _ = state.stdin.flush();
            terminate_child(&mut state.child);
        }
    }

    fn close_async(self: &Arc<Self>) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let process = Arc::clone(self);
            std::thread::spawn(move || process.close_now());
        }
    }
}

impl Drop for ExternalExtensionProcess {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.close_now();
    }
}

fn wait_for_stderr_capture(stderr_done: &mpsc::Receiver<()>) {
    let _ = stderr_done.recv_timeout(Duration::from_millis(100));
}

fn format_child_exit(
    status: std::process::ExitStatus,
    stderr: &Arc<Mutex<String>>,
    stderr_done: &mpsc::Receiver<()>,
) -> String {
    wait_for_stderr_capture(stderr_done);
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
    wait_for_stderr_capture(&state.stderr_done);
    let status = state.child.try_wait().ok().flatten();
    match status {
        Some(status) => format_child_exit(status, &state.stderr, &state.stderr_done),
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

fn is_node_runtime(runner: &str) -> bool {
    Path::new(runner)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let name = name.to_ascii_lowercase();
            name == "node" || name == "node.exe"
        })
        .unwrap_or(false)
}

fn is_typescript_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("ts" | "mts" | "cts")
    )
}

fn is_tsx_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("tsx")
    )
}

fn node_supports_type_stripping(runner: &str) -> bool {
    Command::new(runner)
        .arg("--help")
        .output()
        .map(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            stdout.contains("--experimental-strip-types")
                || stderr.contains("--experimental-strip-types")
        })
        .unwrap_or(false)
}

fn node_reports_runtime(runner: &str) -> bool {
    Command::new(runner)
        .arg("--version")
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('v')
        })
        .unwrap_or(false)
}

fn node_module_loading_diagnostic(runner: &str, resolved_path: &Path) -> Option<String> {
    if !is_node_runtime(runner) || !node_reports_runtime(runner) {
        return None;
    }
    if is_tsx_path(resolved_path) {
        return Some(format!(
            "Failed to load extension: TSX extension {:?} requires Bun or an explicit TypeScript/JSX transpiler; the Node bridge supports native TypeScript type stripping but does not embed jiti",
            resolved_path
        ));
    }
    if is_typescript_path(resolved_path) && !node_supports_type_stripping(runner) {
        return Some(format!(
            "Failed to load extension: Node runtime {:?} does not advertise --experimental-strip-types; use Bun or a TypeScript-capable runner for {:?}",
            runner, resolved_path
        ));
    }
    None
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

fn external_tool_execute(process: Arc<ExternalExtensionProcess>, name: String) -> ToolExecuteFn {
    Arc::new(move |request: ToolExecutionRequest| {
        process
            .request(serde_json::json!({
                "type": "call",
                "kind": "tool",
                "name": name,
                "toolCallId": request.tool_call_id,
                "params": request.params,
                "context": bridge_context(&request.context),
            }))?
            .ok_or_else(|| "Extension tool returned no JSON result".to_string())
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

fn external_native_provider_callback(
    process: Arc<ExternalExtensionProcess>,
    provider: String,
    callback: String,
) -> NativeProviderCallbackFn {
    Arc::new(move |model, context, options| {
        process.request_native_provider_events(&provider, &callback, model, context, options)
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
    runtime: &Arc<Mutex<ExtensionRuntime>>,
) -> Result<(Arc<ExternalExtensionProcess>, serde_json::Value), String> {
    let cwd = resolved_path.parent().unwrap_or_else(|| Path::new("."));
    let mut command = Command::new(runner);
    if is_node_runtime(runner) && node_supports_type_stripping(runner) {
        // Node's native type stripping is the smallest safe equivalent of
        // jiti for ordinary .ts/.mts/.cts imports. It intentionally does not
        // claim to transform JSX or TypeScript syntax requiring emit.
        command.arg("--experimental-strip-types");
    }
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
    let (stderr_done_sender, stderr_done_receiver) = mpsc::channel();
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
        let _ = stderr_done_sender.send(());
    });

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(BridgeFrame::Eof);
                    break;
                }
                Ok(_) => {
                    if line.len() > MAX_BRIDGE_FRAME_BYTES {
                        let _ = sender.send(BridgeFrame::Error(format!(
                            "Extension bridge frame exceeds {} bytes",
                            MAX_BRIDGE_FRAME_BYTES
                        )));
                        break;
                    }
                    if sender.send(BridgeFrame::Line(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(BridgeFrame::Error(error.to_string()));
                    break;
                }
            }
        }
    });
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(10_000));
    let mut stdin = BufWriter::new(stdin);
    let handshake_deadline = Instant::now() + timeout;
    let ready = loop {
        let remaining = handshake_deadline.saturating_duration_since(Instant::now());
        let frame = match receiver.recv_timeout(remaining) {
            Ok(frame) => frame,
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
        let line = match frame {
            BridgeFrame::Line(line) => line,
            BridgeFrame::Eof => {
                let error = format_child_exit_after_parts(
                    &mut child,
                    &stderr_capture,
                    &stderr_done_receiver,
                );
                return Err(format!("Failed to load extension: {error}"));
            }
            BridgeFrame::Error(error) => {
                terminate_child(&mut child);
                return Err(format!(
                    "Failed to load extension: bridge read failed: {error}"
                ));
            }
        };
        let frame: serde_json::Value = serde_json::from_str(line.trim()).map_err(|error| {
            terminate_child(&mut child);
            format!("Failed to load extension: bridge returned invalid JSON: {error}")
        })?;
        if frame.get("type") == Some(&serde_json::Value::String("host_action".into())) {
            if let Err(error) = respond_to_bridge_host_action(runtime, &mut stdin, &frame) {
                terminate_child(&mut child);
                return Err(format!("Failed to load extension: {error}"));
            }
            continue;
        }
        break frame;
    };
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
            stdin,
            frames: receiver,
            stderr: stderr_capture,
            stderr_done: stderr_done_receiver,
            request_timeout: timeout,
        }),
        request_lock: Mutex::new(()),
        next_request_id: AtomicU64::new(1),
        process_id: NEXT_EXTERNAL_PROCESS_ID.fetch_add(1, Ordering::Relaxed),
        closed: AtomicBool::new(false),
        runtime: Arc::clone(runtime),
    });
    let process_for_invalidation = Arc::downgrade(&process);
    if let Ok(runtime_guard) = runtime.lock() {
        let _ = runtime_guard.track_event_bus_subscription(Arc::new(move || {
            if let Some(process) = process_for_invalidation.upgrade() {
                process.close_async();
            }
        }));
    }
    Ok((process, ready))
}

fn format_child_exit_after_parts(
    child: &mut std::process::Child,
    stderr: &Arc<Mutex<String>>,
    stderr_done: &mpsc::Receiver<()>,
) -> String {
    wait_for_stderr_capture(stderr_done);
    let status = child.try_wait().ok().flatten();
    match status {
        Some(status) => format_child_exit(status, stderr, stderr_done),
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
                    name: name.clone(),
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
                    execute: Some(external_tool_execute(Arc::clone(&process), name)),
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
    if let Some(providers) = metadata
        .get("nativeProviders")
        .and_then(serde_json::Value::as_array)
    {
        for provider in providers {
            let name = required_string(provider, "name")?;
            let callback_names = provider
                .get("callbacks")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| format!("Native provider {name:?} is missing callback metadata"))?;
            let mut callbacks = std::collections::BTreeMap::new();
            for callback in callback_names {
                let callback_name = callback.as_str().ok_or_else(|| {
                    format!("Native provider {name:?} has a non-string callback name")
                })?;
                callbacks.insert(
                    callback_name.to_string(),
                    external_native_provider_callback(
                        Arc::clone(&process),
                        name.clone(),
                        callback_name.to_string(),
                    ),
                );
            }
            queue_native_provider_registration(
                runtime,
                PendingNativeProviderRegistration {
                    provider: name.clone(),
                    definition: provider
                        .get("definition")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"id": name})),
                    callbacks,
                    extension_path: extension_path.to_string(),
                },
            );
        }
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
    if let Some(diagnostic) = node_module_loading_diagnostic(&runner, resolved_path) {
        return Err(diagnostic);
    }
    let (process, ready) =
        spawn_external_bridge(extension_path, resolved_path, &runner, timeout_ms, runtime)?;
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

/// Spawn an external extension runner for a resolved entry. JavaScript
/// runtimes use the persistent bridge; other runners retain the legacy
/// one-shot compatibility path used by diagnostics and tests.
pub fn run_external_extension(
    extension_path: &str,
    resolved_path: &Path,
    runner: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<Extension, String> {
    let runner = resolve_external_runner(runner)?;
    let runtime = create_extension_runtime();
    run_external_extension_with_runtime(
        extension_path,
        resolved_path,
        Some(&runner),
        timeout_ms,
        &runtime,
    )
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

/// Load extensions and immediately perform the production-shaped bindCore
/// step against the returned shared runtime. Factories still observe the
/// upstream pre-bind not-initialized behavior; callbacks become live only
/// after the result is assembled.
pub fn load_extensions_with_host_actions(
    paths: &[String],
    cwd: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
    actions: Arc<dyn crate::core::extensions::types::ExtensionHostActions>,
) -> LoadExtensionsResult {
    let result = load_extensions(paths, cwd, runtime, runner);
    result.bind_core_with_actions(actions);
    result
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
        // Fake non-JavaScript runner: a script that records its argv and exits 0.
        let dir = sandbox("fake");
        let entry = dir.join("index.ts");
        fs::write(&entry, "export default () => {}").unwrap();
        let bin = sandbox("bin");
        let node_path = bin.join("runner");
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
    fn node_bridge_loads_typescript_with_relative_typescript_import() {
        if !command_on_path("node") || !node_supports_type_stripping("node") {
            return;
        }
        let dir = sandbox("typescript");
        let helper = dir.join("helper.ts");
        let entry = dir.join("index.ts");
        fs::write(
            &helper,
            r#"export const defaultFlag: string = "loaded-from-ts";"#,
        )
        .unwrap();
        fs::write(
            &entry,
            r#"
import { defaultFlag } from "./helper.ts";

export default (pi: any): void => {
  pi.registerFlag("typescript-fixture", { type: "string", default: defaultFlag });
};
"#,
        )
        .unwrap();

        let extension = run_external_extension("index.ts", &entry, Some("node"), None)
            .expect("Node type stripping should load a .ts extension");
        let flag = extension
            .flags
            .get("typescript-fixture")
            .expect("TypeScript factory should register its flag");
        assert_eq!(flag.default, Some(serde_json::json!("loaded-from-ts")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_bridge_reports_tsx_boundary_deterministically() {
        if !command_on_path("node") {
            return;
        }
        let dir = sandbox("tsx");
        let entry = dir.join("index.tsx");
        fs::write(&entry, "export default (pi: any) => pi;").unwrap();

        let error = run_external_extension("index.tsx", &entry, Some("node"), None)
            .expect_err("Node must reject TSX without an explicit transpiler");
        assert!(error.contains("TSX extension"), "{error}");
        assert!(error.contains("does not embed jiti"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_bridge_reports_unavailable_upstream_virtual_module() {
        if !command_on_path("node") {
            return;
        }
        let dir = sandbox("virtual-module");
        let entry = dir.join("index.js");
        fs::write(
            &entry,
            r#"
import * as tui from "@earendil-works/pi-tui";
export default () => tui;
"#,
        )
        .unwrap();

        let error = run_external_extension("index.js", &entry, Some("node"), None)
            .expect_err("the fixture intentionally has no virtual package installation");
        assert!(
            error.contains("Virtual module \"@earendil-works/pi-tui\" is not resolvable"),
            "{error}"
        );
        assert!(error.contains("does not embed jiti"), "{error}");
        let _ = fs::remove_dir_all(&dir);
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
