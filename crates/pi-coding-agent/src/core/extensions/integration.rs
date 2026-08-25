//! Shared integration between the extension runner and coding-agent modes.
//!
//! The loader/runner deliberately owns extension protocol details.  This
//! module owns the small adapter needed by the agent loop: a host-action state
//! object, extension-tool conversion, and mode-scoped loading policy.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::tools::{AgentTool, AgentToolResult, ToolExecuteFn, ToolUpdateCallback};
use pi_ai::types::{json_tool, ContentBlock};
use serde_json::{json, Value};

use crate::args::Args;
use crate::core::settings::SettingsManager;

use super::loader::{discover_and_load_extensions, load_extensions_with_host_actions};
use super::runner::ExtensionRunner;
use super::types::{ExtensionHostAction, ExtensionHostActions, ExtensionLoadError, RegisteredTool};

#[derive(Debug)]
struct ExtensionHostStateInner {
    session_name: Option<String>,
    active_tools: Vec<String>,
    all_tools: Vec<Value>,
    commands: Vec<Value>,
    thinking_level: String,
    model: Option<Value>,
    scoped_models: Vec<Value>,
    is_idle: bool,
    is_project_trusted: bool,
    signal: Option<Arc<AtomicBool>>,
    tool_signals: BTreeMap<String, Arc<AtomicBool>>,
    has_pending_messages: bool,
    context_usage: Option<Value>,
    system_prompt: String,
    system_prompt_options: Value,
    requested_model: Option<Value>,
    pending_messages: Vec<Value>,
    pending_entries: Vec<Value>,
    pending_actions: Vec<Value>,
    pending_tool_updates: Vec<(Option<String>, Value)>,
    labels: Vec<Value>,
}

impl Default for ExtensionHostStateInner {
    fn default() -> Self {
        Self {
            session_name: None,
            active_tools: Vec::new(),
            all_tools: Vec::new(),
            commands: Vec::new(),
            thinking_level: "medium".to_string(),
            model: None,
            scoped_models: Vec::new(),
            is_idle: true,
            is_project_trusted: true,
            signal: None,
            tool_signals: BTreeMap::new(),
            has_pending_messages: false,
            context_usage: None,
            system_prompt: String::new(),
            system_prompt_options: json!({}),
            requested_model: None,
            pending_messages: Vec::new(),
            pending_entries: Vec::new(),
            pending_actions: Vec::new(),
            pending_tool_updates: Vec::new(),
            labels: Vec::new(),
        }
    }
}

/// Host-owned state shared by an extension bridge and the active mode.
///
/// The mode can consume the queued message/entry requests after a turn, while
/// synchronous getters are served from the same snapshot that the bridge
/// receives for every callback.
#[derive(Clone, Debug, Default)]
pub struct ExtensionHostState {
    inner: Arc<Mutex<ExtensionHostStateInner>>,
}

impl ExtensionHostState {
    pub fn new(session_name: Option<String>, thinking_level: impl Into<String>) -> Self {
        let state = Self::default();
        if let Ok(mut inner) = state.inner.lock() {
            inner.session_name = session_name;
            inner.thinking_level = thinking_level.into();
        }
        state
    }

    /// Replace the synchronous tool/command catalog visible to extension
    /// callbacks.  `active_tools` is intentionally explicit because modes
    /// may disable all tools or only the built-ins.
    pub fn set_catalog(
        &self,
        active_tools: Vec<String>,
        all_tools: Vec<Value>,
        commands: Vec<Value>,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.active_tools = active_tools;
            inner.all_tools = all_tools;
            inner.commands = commands;
        }
    }

    pub fn active_tools(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|inner| inner.active_tools.clone())
            .unwrap_or_default()
    }

    pub fn set_model(&self, model: Option<Value>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.model = model;
        }
    }

    pub fn set_scoped_models(&self, scoped_models: Vec<Value>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.scoped_models = scoped_models;
        }
    }

    pub fn set_idle(&self, is_idle: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.is_idle = is_idle;
        }
    }

    pub fn set_project_trusted(&self, is_project_trusted: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.is_project_trusted = is_project_trusted;
        }
    }

    /// Bind the signal for the currently executing agent tool. The bridge
    /// receives only an `aborted` snapshot; the shared flag still lets an
    /// external `ctx.abort()` action cancel the Rust-side operation.
    pub fn set_signal(&self, signal: Option<Arc<AtomicBool>>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.signal = signal;
        }
    }

    pub fn set_has_pending_messages(&self, has_pending_messages: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.has_pending_messages = has_pending_messages;
        }
    }

    pub fn set_context_usage(&self, context_usage: Option<Value>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.context_usage = context_usage;
        }
    }

    pub fn set_system_prompt(&self, system_prompt: impl Into<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.system_prompt = system_prompt.into();
        }
    }

    pub fn set_system_prompt_options(&self, system_prompt_options: Value) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.system_prompt_options = system_prompt_options;
        }
    }

    /// Start a tool callback boundary and clear updates left by a prior call.
    pub fn begin_tool_execution(&self, tool_call_id: &str, signal: Option<Arc<AtomicBool>>) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(signal) = signal {
                inner.tool_signals.insert(tool_call_id.to_string(), signal);
            } else {
                inner.tool_signals.remove(tool_call_id);
            }
            inner
                .pending_tool_updates
                .retain(|(call_id, _)| call_id.as_deref() != Some(tool_call_id));
        }
    }

    /// End a tool callback boundary, returning bridge updates in protocol
    /// arrival order. Clearing the signal prevents it leaking into the next
    /// handler or tool request.
    pub fn end_tool_execution(&self, tool_call_id: &str) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| {
                inner.tool_signals.remove(tool_call_id);
                let mut updates = Vec::new();
                let mut remaining = Vec::new();
                for (call_id, update) in std::mem::take(&mut inner.pending_tool_updates) {
                    if call_id.as_deref() == Some(tool_call_id) {
                        updates.push(update);
                    } else {
                        remaining.push((call_id, update));
                    }
                }
                inner.pending_tool_updates = remaining;
                updates
            })
            .unwrap_or_default()
    }

    /// Drain asynchronous messages requested by extensions.  The current
    /// mode owns delivery semantics; retaining them here prevents a bridge
    /// callback from recursively entering the agent loop.
    pub fn drain_pending_messages(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.pending_messages))
            .unwrap_or_default()
    }

    pub fn drain_pending_entries(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.pending_entries))
            .unwrap_or_default()
    }

    pub fn drain_pending_actions(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.pending_actions))
            .unwrap_or_default()
    }

    pub fn drain_pending_tool_updates(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| {
                std::mem::take(&mut inner.pending_tool_updates)
                    .into_iter()
                    .map(|(_, update)| update)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn snapshot_value_for(&self, tool_call_id: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("extension host state lock");
        let signal = tool_call_id
            .and_then(|tool_call_id| inner.tool_signals.get(tool_call_id))
            .or(inner.signal.as_ref())
            .map(|signal| {
                json!({
                    "aborted": signal.load(Ordering::Acquire),
                })
            });
        json!({
            "sessionName": inner.session_name,
            "activeTools": inner.active_tools,
            "allTools": inner.all_tools,
            "commands": inner.commands,
            "thinkingLevel": inner.thinking_level,
            "model": inner.model,
            "scopedModels": inner.scoped_models,
            "isIdle": inner.is_idle,
            "isProjectTrusted": inner.is_project_trusted,
            "signal": signal,
            "hasPendingMessages": inner.has_pending_messages,
            "contextUsage": inner.context_usage,
            "systemPrompt": inner.system_prompt,
            "systemPromptOptions": inner.system_prompt_options,
        })
    }

    fn snapshot_value(&self) -> Value {
        self.snapshot_value_for(None)
    }

    fn dispatch_action(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Extension host state lock poisoned".to_string())?;
        match action {
            ExtensionHostAction::SendMessage => {
                inner.pending_messages.push(json!({
                    "type": "send_message",
                    "message": args.get("message").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }));
                Ok(Value::Null)
            }
            ExtensionHostAction::SendUserMessage => {
                inner.pending_messages.push(json!({
                    "type": "send_user_message",
                    "content": args.get("content").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }));
                Ok(Value::Null)
            }
            ExtensionHostAction::AppendEntry => {
                inner.pending_entries.push(json!({
                    "customType": args.get("customType").cloned().unwrap_or(Value::Null),
                    "data": args.get("data").cloned().unwrap_or(Value::Null),
                }));
                Ok(Value::Null)
            }
            ExtensionHostAction::SetSessionName => {
                inner.session_name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                Ok(Value::Null)
            }
            ExtensionHostAction::GetSessionName => Ok(inner
                .session_name
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null)),
            ExtensionHostAction::SetLabel => {
                inner.labels.push(args.clone());
                Ok(Value::Null)
            }
            ExtensionHostAction::GetActiveTools => Ok(Value::Array(
                inner
                    .active_tools
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )),
            ExtensionHostAction::GetAllTools => Ok(Value::Array(inner.all_tools.clone())),
            ExtensionHostAction::SetActiveTools => {
                inner.active_tools = args
                    .get("toolNames")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect();
                Ok(Value::Null)
            }
            ExtensionHostAction::GetCommands => Ok(Value::Array(inner.commands.clone())),
            ExtensionHostAction::SetModel => {
                inner.requested_model = args.get("model").cloned();
                inner.model = inner.requested_model.clone();
                Ok(Value::Bool(true))
            }
            ExtensionHostAction::GetThinkingLevel => {
                Ok(Value::String(inner.thinking_level.clone()))
            }
            ExtensionHostAction::SetThinkingLevel => {
                if let Some(level) = args.get("level").and_then(Value::as_str) {
                    inner.thinking_level = level.to_string();
                }
                Ok(Value::Null)
            }
            ExtensionHostAction::GetModel => Ok(inner.model.clone().unwrap_or(Value::Null)),
            ExtensionHostAction::GetScopedModels => Ok(Value::Array(inner.scoped_models.clone())),
            ExtensionHostAction::IsIdle => Ok(Value::Bool(inner.is_idle)),
            ExtensionHostAction::IsProjectTrusted => Ok(Value::Bool(inner.is_project_trusted)),
            ExtensionHostAction::GetSignal => {
                let signal = args
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|tool_call_id| inner.tool_signals.get(tool_call_id))
                    .or(inner.signal.as_ref());
                Ok(signal
                    .map(|signal| json!({"aborted": signal.load(Ordering::Acquire)}))
                    .unwrap_or(Value::Null))
            }
            ExtensionHostAction::Abort => {
                let tool_call_id = args.get("toolCallId").and_then(Value::as_str);
                if let Some(signal) = tool_call_id
                    .and_then(|tool_call_id| inner.tool_signals.get(tool_call_id))
                    .or(inner.signal.as_ref())
                {
                    signal.store(true, Ordering::Release);
                }
                let mut action = serde_json::Map::new();
                action.insert("type".to_string(), Value::String("abort".to_string()));
                if let Some(tool_call_id) = tool_call_id {
                    action.insert(
                        "toolCallId".to_string(),
                        Value::String(tool_call_id.to_string()),
                    );
                }
                inner.pending_actions.push(Value::Object(action));
                Ok(Value::Null)
            }
            ExtensionHostAction::HasPendingMessages => Ok(Value::Bool(inner.has_pending_messages)),
            ExtensionHostAction::Shutdown => {
                let mut action = serde_json::Map::new();
                action.insert("type".to_string(), Value::String("shutdown".to_string()));
                if let Some(tool_call_id) = args.get("toolCallId").and_then(Value::as_str) {
                    action.insert(
                        "toolCallId".to_string(),
                        Value::String(tool_call_id.to_string()),
                    );
                }
                inner.pending_actions.push(Value::Object(action));
                Ok(Value::Null)
            }
            ExtensionHostAction::GetContextUsage => {
                Ok(inner.context_usage.clone().unwrap_or(Value::Null))
            }
            ExtensionHostAction::Compact => {
                let mut action = serde_json::Map::new();
                action.insert("type".to_string(), Value::String("compact".to_string()));
                action.insert(
                    "options".to_string(),
                    args.get("options").cloned().unwrap_or(Value::Null),
                );
                if let Some(tool_call_id) = args.get("toolCallId").and_then(Value::as_str) {
                    action.insert(
                        "toolCallId".to_string(),
                        Value::String(tool_call_id.to_string()),
                    );
                }
                inner.pending_actions.push(Value::Object(action));
                Ok(Value::Null)
            }
            ExtensionHostAction::GetSystemPrompt => Ok(Value::String(inner.system_prompt.clone())),
            ExtensionHostAction::GetSystemPromptOptions => Ok(inner.system_prompt_options.clone()),
            ExtensionHostAction::ToolUpdate => {
                let tool_call_id = args
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                inner.pending_tool_updates.push((
                    tool_call_id,
                    args.get("result").cloned().unwrap_or(Value::Null),
                ));
                Ok(Value::Null)
            }
        }
    }
}

impl ExtensionHostActions for ExtensionHostState {
    fn dispatch(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String> {
        self.dispatch_action(action, args)
    }

    fn snapshot(&self) -> Value {
        self.snapshot_value()
    }

    fn snapshot_for(&self, request: &Value) -> Value {
        self.snapshot_value_for(request.get("toolCallId").and_then(Value::as_str))
    }
}

/// A loaded, mode-scoped extension runtime.
#[derive(Clone)]
pub struct LoadedExtensions {
    pub runner: Arc<ExtensionRunner>,
    pub host: Arc<ExtensionHostState>,
    pub errors: Vec<ExtensionLoadError>,
}

/// Load extensions using the same project/global/explicit path policy as the
/// upstream resource loader.  `--no-extensions` retains only explicit `-e`
/// paths, matching the CLI contract.
#[allow(clippy::too_many_arguments)] // explicit mode/session binding mirrors the upstream runtime context
pub fn load_for_mode(
    args: &Args,
    settings: &SettingsManager,
    cwd: &str,
    agent_dir: &str,
    mode: &str,
    has_ui: bool,
    session_name: Option<String>,
    thinking_level: impl Into<String>,
) -> LoadedExtensions {
    let host = Arc::new(ExtensionHostState::new(session_name, thinking_level));
    host.set_system_prompt_options(json!({"cwd": cwd}));
    let mut configured_paths = args.extensions.clone();
    if !args.no_extensions {
        configured_paths.extend(settings.get_extension_paths());
    }
    let result = if args.no_extensions {
        load_extensions_with_host_actions(&configured_paths, cwd, None, None, host.clone())
    } else {
        let agent_dir = agent_dir.to_string();
        let result = discover_and_load_extensions(&configured_paths, cwd, &agent_dir, None, None);
        result.bind_core_with_actions(host.clone());
        result
    };
    let mut runner = ExtensionRunner::new(result.extensions, result.runtime, cwd.to_string());
    runner.set_ui_context(mode, has_ui);
    let runner = Arc::new(runner);
    LoadedExtensions {
        runner,
        host,
        errors: result.errors,
    }
}

/// Add live extension tools to a mode's base tool vector and publish the
/// resulting catalog to synchronous extension getters.
pub fn install_tools(
    loaded: &LoadedExtensions,
    tools: &mut Vec<AgentTool>,
    include_extensions: bool,
) {
    let mut all_tools = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.tool.name,
                "description": tool.tool.description,
                "parameters": tool.tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    let mut active_tools = tools
        .iter()
        .map(|tool| tool.tool.name.clone())
        .collect::<Vec<_>>();

    let extension_tools = extension_agent_tools(loaded.runner.clone(), loaded.host.clone());
    for tool in &extension_tools {
        all_tools.push(json!({
            "name": tool.tool.name,
            "description": tool.tool.description,
            "parameters": tool.tool.parameters,
        }));
        if include_extensions {
            active_tools.push(tool.tool.name.clone());
        }
    }
    let mut command_runner = loaded.runner.as_ref().clone();
    let commands = command_runner
        .get_registered_commands()
        .into_iter()
        .map(|command| {
            json!({
                "name": command.invocation_name,
                "description": command.description,
            })
        })
        .collect();
    loaded.host.set_catalog(active_tools, all_tools, commands);
    if include_extensions {
        tools.extend(extension_tools);
    }
}

fn extension_agent_tools(
    runner: Arc<ExtensionRunner>,
    host: Arc<ExtensionHostState>,
) -> Vec<AgentTool> {
    runner
        .get_all_registered_tools()
        .into_iter()
        .map(|registered| extension_agent_tool(registered, runner.clone(), host.clone()))
        .collect()
}

fn extension_agent_tool(
    registered: RegisteredTool,
    runner: Arc<ExtensionRunner>,
    host: Arc<ExtensionHostState>,
) -> AgentTool {
    let tool_name = registered.name.clone();
    let tool = json_tool(
        &registered.name,
        &registered.description,
        &registered.parameters,
    );
    let execute: ToolExecuteFn = Arc::new(
        move |tool_call_id,
              params,
              signal: Option<Arc<AtomicBool>>,
              on_update: Option<ToolUpdateCallback>| {
            let runner = runner.clone();
            let host = host.clone();
            let tool_name = tool_name.clone();
            Box::pin(async move {
                if signal
                    .as_ref()
                    .is_some_and(|signal| signal.load(Ordering::Acquire))
                {
                    return Err("Operation aborted".to_string());
                }
                let before = host.active_tools();
                host.begin_tool_execution(&tool_call_id, signal.clone());
                let execution_tool_call_id = tool_call_id.clone();
                let execution = tokio::task::spawn_blocking(move || {
                    runner.execute_tool(&tool_name, &execution_tool_call_id, params)
                })
                .await;
                let pending_updates = host.end_tool_execution(&tool_call_id);
                let value =
                    execution.map_err(|error| format!("extension tool task failed: {error}"))??;
                let mut result = extension_tool_result(value)?;
                let after = host.active_tools();
                let before_set = before.iter().map(String::as_str).collect::<BTreeSet<_>>();
                if before
                    .iter()
                    .all(|name| after.iter().any(|value| value == name))
                {
                    let mut seen = BTreeSet::new();
                    result
                        .added_tool_names
                        .retain(|name| seen.insert(name.clone()));
                    for name in after
                        .iter()
                        .filter(|name| !before_set.contains(name.as_str()))
                    {
                        if seen.insert(name.clone()) {
                            result.added_tool_names.push(name.clone());
                        }
                    }
                }
                if let Some(on_update) = on_update {
                    for update in pending_updates {
                        let update = extension_tool_result(update)?;
                        on_update(&update);
                    }
                    on_update(&result);
                }
                Ok(result)
            })
        },
    );
    AgentTool::new(
        tool,
        format!(
            "Extension: {registered_name}",
            registered_name = registered.name
        ),
        execute,
    )
}

fn extension_tool_result(value: Value) -> Result<AgentToolResult, String> {
    let value = normalize_tool_result(value);
    let content = match value.get("content") {
        None | Some(Value::Null) => Vec::new(),
        Some(content) => match serde_json::from_value::<Vec<ContentBlock>>(content.clone()) {
            Ok(content) => content,
            Err(_) => content
                .as_str()
                .map(|text| vec![ContentBlock::text(text)])
                .unwrap_or_else(|| vec![ContentBlock::text(compact_json(&value))]),
        },
    };
    let details = value.get("details").cloned();
    let usage = value
        .get("usage")
        .cloned()
        .and_then(|usage| serde_json::from_value(usage).ok());
    let added_tool_names = value
        .get("addedToolNames")
        .or_else(|| value.get("added_tool_names"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    let terminate = value
        .get("terminate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = AgentToolResult {
        content,
        details,
        usage,
        added_tool_names,
        terminate,
    };
    if value
        .get("isError")
        .or_else(|| value.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(extension_tool_error(&value, &result));
    }
    Ok(result)
}

fn normalize_tool_result(value: Value) -> Value {
    let Value::Object(mut outer) = value else {
        return value;
    };
    let Some(inner) = outer.remove("result") else {
        return Value::Object(outer);
    };
    let Value::Object(mut inner) = inner else {
        outer.insert("result".to_string(), inner);
        return Value::Object(outer);
    };
    for (key, value) in outer {
        inner.entry(key).or_insert(value);
    }
    Value::Object(inner)
}

fn extension_tool_error(value: &Value, result: &AgentToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        compact_json(value)
    } else {
        text
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "Extension tool completed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::{SettingsManager, SettingsMap};
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pi-extension-integration-{name}-{}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn mode_loader_exposes_live_extension_tools_and_host_snapshot() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = fixture_root("tool");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture directory");
        let extension = root.join("index.js");
        std::fs::write(
            &extension,
            r#"export default function (pi) {
  pi.registerTool({
    name: "mode-tool",
    description: "mode integration fixture",
    parameters: { type: "object", properties: {} },
    execute: async (toolCallId, params, signal, onUpdate, ctx) => {
      const before = {
        session: ctx.getSessionName() ?? null,
        active: ctx.getActiveTools(),
        all: ctx.getAllTools().map((tool) => tool.name),
        commands: ctx.getCommands().map((command) => command.name),
        thinking: ctx.getThinkingLevel(),
        propertyThinking: ctx.thinkingLevel,
      };
      ctx.sendMessage({ customType: "tool-message", content: "from-tool" });
      ctx.sendUserMessage("from-tool-user");
      ctx.appendEntry("tool-entry", { source: "fixture" });
      ctx.setSessionName("from-context");
      ctx.setActiveTools(["mode-tool", "context-added"]);
      ctx.setThinkingLevel("high");
      const modelSet = await ctx.setModel({ id: "fixture-model" });
      return {
        content: [{ type: "text", text: `${toolCallId}:${params.value}` }],
        details: {
          source: "fixture",
          before,
          after: {
            session: ctx.getSessionName(),
            active: ctx.getActiveTools(),
            thinking: ctx.getThinkingLevel(),
            propertyThinking: ctx.thinkingLevel,
            modelSet,
          },
        },
      };
    },
  });
  pi.registerCommand("mode-command", { description: "command", handler: async () => ({ ok: true }) });
}"#,
        )
        .expect("write extension fixture");

        let args = Args {
            extensions: vec![extension.to_string_lossy().into_owned()],
            no_extensions: true,
            ..Default::default()
        };
        let loaded = load_for_mode(
            &args,
            &SettingsManager::in_memory(SettingsMap::new()),
            &root.to_string_lossy(),
            &root.join("agent").to_string_lossy(),
            "print",
            false,
            None,
            "medium",
        );
        assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);
        let mut tools = Vec::new();
        install_tools(&loaded, &mut tools, true);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool.name, "mode-tool");
        assert_eq!(loaded.host.snapshot()["allTools"][0]["name"], "mode-tool");
        assert_eq!(
            loaded.host.snapshot()["commands"][0]["name"],
            "mode-command"
        );

        let result = (tools[0].execute)("call-1".to_string(), json!({"value": 7}), None, None)
            .await
            .expect("extension tool execution");
        assert_eq!(result.content, vec![ContentBlock::text("call-1:7")]);
        assert_eq!(
            result.details,
            Some(json!({
                "source": "fixture",
                "before": {
                    "session": null,
                    "active": ["mode-tool"],
                    "all": ["mode-tool"],
                    "commands": ["mode-command"],
                    "thinking": "medium",
                    "propertyThinking": "medium",
                },
                "after": {
                    "session": "from-context",
                    "active": ["mode-tool", "context-added"],
                    "thinking": "high",
                    "propertyThinking": "high",
                    "modelSet": true,
                },
            }))
        );
        assert_eq!(result.added_tool_names, vec!["context-added"]);
        assert_eq!(loaded.host.snapshot()["sessionName"], "from-context");
        assert_eq!(loaded.host.snapshot()["thinkingLevel"], "high");
        assert_eq!(
            loaded.host.drain_pending_messages(),
            vec![
                json!({
                    "type": "send_message",
                    "message": {"customType": "tool-message", "content": "from-tool"},
                    "options": null,
                }),
                json!({
                    "type": "send_user_message",
                    "content": "from-tool-user",
                    "options": null,
                }),
            ]
        );
        assert_eq!(
            loaded.host.drain_pending_entries(),
            vec![json!({
                "customType": "tool-entry",
                "data": {"source": "fixture"},
            })]
        );

        loaded.runner.invalidate(Some("test complete"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn external_tool_context_forwards_signal_updates_and_control_actions() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = fixture_root("context-actions");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture directory");
        let extension = root.join("index.js");
        std::fs::write(
            &extension,
            r#"export default function (pi) {
  pi.registerTool({
    name: "context-tool",
    description: "context action fixture",
    parameters: { type: "object", properties: {} },
    execute: async (toolCallId, params, signal, onUpdate, ctx) => {
      signal.throwIfAborted();
      const before = {
        model: ctx.model,
        scopedModels: ctx.scopedModels,
        idle: ctx.isIdle(),
        trusted: ctx.isProjectTrusted(),
        contextSignal: ctx.signal?.aborted ?? null,
        toolSignal: signal.aborted,
        pending: ctx.hasPendingMessages(),
        usage: ctx.getContextUsage(),
        prompt: ctx.getSystemPrompt(),
        options: ctx.getSystemPromptOptions(),
      };
      onUpdate({ content: [{ type: "text", text: "partial" }], details: { sequence: 1 } });
      ctx.abort();
      ctx.compact({ customInstructions: "tool compact" });
      ctx.shutdown();
      return {
        content: [{ type: "text", text: `${toolCallId}:${params.value}` }],
        details: { before, afterAborted: signal.aborted },
      };
    },
  });
}"#,
        )
        .expect("write extension fixture");

        let args = Args {
            extensions: vec![extension.to_string_lossy().into_owned()],
            no_extensions: true,
            ..Default::default()
        };
        let loaded = load_for_mode(
            &args,
            &SettingsManager::in_memory(SettingsMap::new()),
            &root.to_string_lossy(),
            &root.join("agent").to_string_lossy(),
            "print",
            false,
            Some("fixture-session".to_string()),
            "medium",
        );
        assert!(loaded.errors.is_empty(), "load errors: {:?}", loaded.errors);
        loaded
            .host
            .set_model(Some(json!({"provider": "fixture", "id": "model-1"})));
        loaded.host.set_scoped_models(vec![json!({
            "model": {"provider": "fixture", "id": "scoped-1"},
            "thinkingLevel": "high",
        })]);
        loaded.host.set_idle(false);
        loaded.host.set_project_trusted(false);
        loaded.host.set_has_pending_messages(true);
        loaded.host.set_context_usage(Some(json!({
            "tokens": 12,
            "contextWindow": 100,
            "percent": 0.12,
        })));
        loaded.host.set_system_prompt("fixture system prompt");
        loaded.host.set_system_prompt_options(json!({
            "cwd": root.to_string_lossy(),
            "selectedTools": ["context-tool"],
        }));

        let mut tools = Vec::new();
        install_tools(&loaded, &mut tools, true);
        assert_eq!(tools.len(), 1);
        let signal = Arc::new(AtomicBool::new(false));
        let updates = Arc::new(Mutex::new(Vec::<AgentToolResult>::new()));
        let updates_for_callback = Arc::clone(&updates);
        let result = (tools[0].execute)(
            "call-context".to_string(),
            json!({"value": 7}),
            Some(Arc::clone(&signal)),
            Some(Arc::new(move |update| {
                updates_for_callback
                    .lock()
                    .expect("update lock")
                    .push(update.clone());
            })),
        )
        .await
        .expect("extension tool execution");

        assert_eq!(result.content, vec![ContentBlock::text("call-context:7")]);
        let details = result.details.as_ref().expect("tool details");
        assert_eq!(
            details["before"],
            json!({
                "model": {"provider": "fixture", "id": "model-1"},
                "scopedModels": [{
                    "model": {"provider": "fixture", "id": "scoped-1"},
                    "thinkingLevel": "high",
                }],
                "idle": false,
                "trusted": false,
                "contextSignal": false,
                "toolSignal": false,
                "pending": true,
                "usage": {"tokens": 12, "contextWindow": 100, "percent": 0.12},
                "prompt": "fixture system prompt",
                "options": {
                    "cwd": root.to_string_lossy(),
                    "selectedTools": ["context-tool"],
                },
            })
        );
        assert_eq!(details["afterAborted"], true);
        assert!(signal.load(Ordering::Acquire));

        let updates = updates.lock().expect("update lock");
        assert_eq!(updates.len(), 2, "partial update must precede final update");
        assert_eq!(updates[0].content, vec![ContentBlock::text("partial")]);
        assert_eq!(updates[1].content, result.content);
        drop(updates);

        assert_eq!(
            loaded.host.drain_pending_actions(),
            vec![
                json!({"type": "abort", "toolCallId": "call-context"}),
                json!({"type": "compact", "options": {"customInstructions": "tool compact"}, "toolCallId": "call-context"}),
                json!({"type": "shutdown", "toolCallId": "call-context"}),
            ]
        );
        assert!(loaded.host.snapshot()["signal"].is_null());
        loaded
            .runner
            .invalidate(Some("context action test complete"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn extension_tool_result_maps_upstream_fields_and_nested_bridge_result() {
        let result = extension_tool_result(json!({
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "details": {"source": "fixture"},
                "addedToolNames": ["one", "two"],
                "terminate": true,
            },
            "details": {"outer": true},
        }))
        .expect("valid extension result");

        assert_eq!(result.content, vec![ContentBlock::text("ok")]);
        assert_eq!(result.details, Some(json!({"source": "fixture"})));
        assert_eq!(result.added_tool_names, vec!["one", "two"]);
        assert!(result.terminate);
    }

    #[test]
    fn extension_tool_result_accepts_text_content_and_reports_error_results() {
        let result = extension_tool_result(json!({"content": "plain text"}))
            .expect("string content should be adapted to a text block");
        assert_eq!(result.content, vec![ContentBlock::text("plain text")]);

        let error = extension_tool_result(json!({
            "content": [{"type": "text", "text": "tool failed"}],
            "isError": true,
        }))
        .expect_err("explicit bridge error results must fail the Rust tool call");
        assert_eq!(error, "tool failed");
    }
}
