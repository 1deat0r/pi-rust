//! Shared integration between the extension runner and coding-agent modes.
//!
//! The loader/runner deliberately owns extension protocol details.  This
//! module owns the small adapter needed by the agent loop: a host-action state
//! object, extension-tool conversion, and mode-scoped loading policy.

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

#[derive(Debug, Default)]
struct ExtensionHostStateInner {
    session_name: Option<String>,
    active_tools: Vec<String>,
    all_tools: Vec<Value>,
    commands: Vec<Value>,
    thinking_level: String,
    requested_model: Option<Value>,
    pending_messages: Vec<Value>,
    pending_entries: Vec<Value>,
    labels: Vec<Value>,
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

    fn snapshot_value(&self) -> Value {
        let inner = self.inner.lock().expect("extension host state lock");
        json!({
            "sessionName": inner.session_name,
            "activeTools": inner.active_tools,
            "allTools": inner.all_tools,
            "commands": inner.commands,
            "thinkingLevel": inner.thinking_level,
        })
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
                let mut result = tokio::task::spawn_blocking(move || {
                    runner.execute_tool(&tool_name, &tool_call_id, params)
                })
                .await
                .map_err(|error| format!("extension tool task failed: {error}"))?
                .map(extension_tool_result)
                .map_err(|error| error.to_string())?;
                let after = host.active_tools();
                let before_set = before.iter().collect::<BTreeSet<_>>();
                if before
                    .iter()
                    .all(|name| after.iter().any(|value| value == name))
                {
                    for name in after.iter().filter(|name| !before_set.contains(name)) {
                        if !result.added_tool_names.contains(name) {
                            result.added_tool_names.push(name.clone());
                        }
                    }
                }
                if let Some(on_update) = on_update {
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

fn extension_tool_result(value: Value) -> AgentToolResult {
    let value = value
        .get("result")
        .filter(|_| value.get("content").is_none())
        .cloned()
        .unwrap_or(value);
    let content = value
        .get("content")
        .and_then(|content| serde_json::from_value::<Vec<ContentBlock>>(content.clone()).ok())
        .unwrap_or_else(|| vec![ContentBlock::text(compact_json(&value))]);
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
    AgentToolResult {
        content,
        details,
        usage,
        added_tool_names,
        terminate,
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
    execute: async (toolCallId, params) => ({
      content: [{ type: "text", text: `${toolCallId}:${params.value}` }],
      details: { source: "fixture" },
    }),
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
        assert_eq!(result.details, Some(json!({"source": "fixture"})));

        loaded.runner.invalidate(Some("test complete"));
        let _ = std::fs::remove_dir_all(root);
    }
}
