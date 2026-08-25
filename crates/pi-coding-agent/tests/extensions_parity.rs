use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use pi_coding_agent::core::extensions::integration::ExtensionHostState;
use pi_coding_agent::core::extensions::loader::{
    create_extension_runtime, discover_extensions_in_dir, load_extension_from_factory,
    load_extensions, load_extensions_with_host_actions, run_external_extension,
};
use pi_coding_agent::core::extensions::runner::{
    ExtensionRunner, InputEventResult, KeybindingsConfig,
};
use pi_coding_agent::core::extensions::types::{
    Extension, ExtensionContext, ExtensionHostAction, ExtensionHostActions, ExtensionRuntime,
    HandlerFn, InputAction, SourceInfo, STALE_MESSAGE,
};
use pi_coding_agent::core::extensions::wrapper::{
    wrap_registered_tool, WrappedToolCall, WrappedToolResult,
};
use serde_json::{json, Value};

fn handler(
    f: impl Fn(&ExtensionContext, &Value) -> Result<Option<Value>, String> + Send + Sync + 'static,
) -> HandlerFn {
    Arc::new(f)
}

fn fixture_extension(path: &str) -> Extension {
    Extension {
        path: path.to_string(),
        resolved_path: path.to_string(),
        source_info: SourceInfo::synthetic(path, "fixture", None),
        ..Extension::default()
    }
}

fn runner_with(extensions: Vec<Extension>) -> ExtensionRunner {
    ExtensionRunner::new(
        extensions,
        create_extension_runtime(),
        "/fixture/project".to_string(),
    )
}

#[derive(Default)]
struct RecordingHost {
    calls: Mutex<Vec<(ExtensionHostAction, Value)>>,
    active_tools: Mutex<Vec<String>>,
    session_name: Mutex<Option<String>>,
    thinking_level: Mutex<String>,
}

impl ExtensionHostActions for RecordingHost {
    fn dispatch(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String> {
        self.calls
            .lock()
            .expect("host calls lock")
            .push((action, args.clone()));
        match action {
            ExtensionHostAction::SendMessage | ExtensionHostAction::SendUserMessage => {
                Ok(Value::Null)
            }
            ExtensionHostAction::AppendEntry
            | ExtensionHostAction::SetLabel
            | ExtensionHostAction::SetSessionName
            | ExtensionHostAction::SetActiveTools
            | ExtensionHostAction::SetThinkingLevel => {
                if action == ExtensionHostAction::SetSessionName {
                    let name = args
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "missing session name".to_string())?;
                    *self.session_name.lock().expect("session name lock") = Some(name.into());
                }
                if action == ExtensionHostAction::SetActiveTools {
                    let tools = args
                        .get("toolNames")
                        .and_then(Value::as_array)
                        .ok_or_else(|| "missing active tools".to_string())?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    *self.active_tools.lock().expect("active tools lock") = tools;
                }
                if action == ExtensionHostAction::SetThinkingLevel {
                    let level = args
                        .get("level")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "missing thinking level".to_string())?;
                    *self.thinking_level.lock().expect("thinking level lock") = level.into();
                }
                Ok(Value::Null)
            }
            ExtensionHostAction::GetSessionName => Ok(json!(self
                .session_name
                .lock()
                .expect("session name lock")
                .clone())),
            ExtensionHostAction::GetActiveTools => Ok(json!(self
                .active_tools
                .lock()
                .expect("active tools lock")
                .clone())),
            ExtensionHostAction::GetAllTools => Ok(json!([{
                "name": "bridge-tool",
                "description": "bridge tool",
                "parameters": {},
            }])),
            ExtensionHostAction::GetCommands => Ok(json!([{
                "name": "bridge",
                "description": "Bridge command",
            }])),
            ExtensionHostAction::SetModel => Ok(Value::Bool(true)),
            ExtensionHostAction::GetThinkingLevel => Ok(json!(self
                .thinking_level
                .lock()
                .expect("thinking level lock")
                .clone())),
            ExtensionHostAction::GetModel
            | ExtensionHostAction::GetScopedModels
            | ExtensionHostAction::IsIdle
            | ExtensionHostAction::IsProjectTrusted
            | ExtensionHostAction::GetSignal
            | ExtensionHostAction::Abort
            | ExtensionHostAction::HasPendingMessages
            | ExtensionHostAction::Shutdown
            | ExtensionHostAction::GetContextUsage
            | ExtensionHostAction::Compact
            | ExtensionHostAction::GetSystemPrompt
            | ExtensionHostAction::GetSystemPromptOptions
            | ExtensionHostAction::ToolUpdate => Ok(Value::Null),
        }
    }

    fn snapshot(&self) -> Value {
        json!({
            "sessionName": self.session_name.lock().expect("session name lock").clone(),
            "activeTools": self.active_tools.lock().expect("active tools lock").clone(),
            "allTools": [{
                "name": "bridge-tool",
                "description": "bridge tool",
                "parameters": {},
            }],
            "commands": [{
                "name": "bridge",
                "description": "Bridge command",
            }],
            "thinkingLevel": self.thinking_level.lock().expect("thinking lock").clone(),
        })
    }
}

#[test]
fn factory_registration_dispatch_and_renderer_order_match_upstream() {
    let runtime = create_extension_runtime();
    let extension = load_extension_from_factory(
        |api| {
            api.on(
                "input",
                handler(|_, event| {
                    Ok(Some(json!({
                        "action": "transform",
                        "text": format!("{}[first]", event["text"].as_str().unwrap()),
                    })))
                }),
            )?;
            api.register_command(
                "echo",
                Some("Echo fixture arguments".to_string()),
                handler(|_, event| Ok(Some(json!({"args": event["args"].clone()})))),
            )?;
            api.register_message_renderer(
                "fixture",
                Arc::new(|message, _options| Ok(Some(json!({"message": message.clone()})))),
            )?;
            api.register_entry_renderer(
                "fixture-entry",
                Arc::new(|entry, _options| Ok(Some(json!({"entry": entry.clone()})))),
            )?;
            api.register_markdown_transformer(Arc::new(|markdown, context| {
                Ok(format!(
                    "{}:{}:{}",
                    markdown, context.message_type, context.available_width
                ))
            }))?;
            Ok(())
        },
        "/fixture/project",
        runtime,
        "<inline:parity>",
    )
    .expect("fixture factory must load");

    let mut runner = runner_with(vec![extension]);
    let command = runner.get_command("echo").expect("command is registered");
    assert_eq!(
        command.description.as_deref(),
        Some("Echo fixture arguments")
    );
    assert_eq!(
        runner
            .execute_command("echo", "one two")
            .expect("command dispatch must succeed"),
        Some(json!({"args": "one two"}))
    );

    let input = runner.emit_input("hello", None, "interactive", None);
    assert_eq!(input.action, InputAction::Transform);
    assert_eq!(input.text.as_deref(), Some("hello[first]"));

    assert_eq!(
        runner
            .render_message("fixture", &json!({"value": 7}), &json!({"expanded": true}))
            .expect("message renderer must not fail"),
        Some(json!({"message": {"value": 7}}))
    );
    assert_eq!(
        runner
            .render_entry(
                "fixture-entry",
                &json!({"id": "entry-1"}),
                &json!({"expanded": false}),
            )
            .expect("entry renderer must not fail"),
        Some(json!({"entry": {"id": "entry-1"}}))
    );
    assert_eq!(
        runner.apply_markdown_transformers(
            "body",
            &pi_coding_agent::core::extensions::types::MarkdownTransformContext {
                message_type: "assistant".to_string(),
                is_streaming: false,
                available_width: 78,
            },
        ),
        "body:assistant:78"
    );
}

#[test]
fn hook_failures_are_isolated_and_chains_continue_deterministically() {
    let mut throwing = fixture_extension("throwing.ts");
    throwing.handlers.insert(
        "input".to_string(),
        vec![handler(|_, _| Err("input boom".to_string()))],
    );
    throwing.handlers.insert(
        "before_provider_headers".to_string(),
        vec![handler(|_, _| panic!("header panic"))],
    );
    throwing.handlers.insert(
        "context".to_string(),
        vec![handler(|_, _| Err("context boom".to_string()))],
    );

    let mut good = fixture_extension("good.ts");
    good.handlers.insert(
        "input".to_string(),
        vec![handler(|_, event| {
            Ok(Some(json!({
                "action": "transform",
                "text": format!("{}[good]", event["text"].as_str().unwrap()),
            })))
        })],
    );
    good.handlers.insert(
        "before_provider_headers".to_string(),
        vec![handler(|_, _| Ok(Some(json!({"X-Good": "yes"}))))],
    );
    good.handlers.insert(
        "context".to_string(),
        vec![handler(|_, event| {
            Ok(Some(json!({"messages": event["messages"].clone()})))
        })],
    );

    let runner = runner_with(vec![throwing, good]);
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let errors_for_listener = Arc::clone(&errors);
    let _unsubscribe = runner.on_error(Arc::new(move |error| {
        errors_for_listener
            .lock()
            .expect("fixture lock")
            .push(format!("{}:{}", error.event, error.error));
    }));

    let input = runner.emit_input("hello", None, "interactive", None);
    assert_eq!(input, InputEventResult::transform("hello[good]", None));

    let headers = runner.emit_before_provider_headers(json!({"User-Agent": "fixture/1"}));
    assert_eq!(headers, json!({"User-Agent": "fixture/1", "X-Good": "yes"}));

    let messages = runner.emit_context(json!([{"role": "user", "text": "hello"}]));
    assert_eq!(messages, json!([{"role": "user", "text": "hello"}]));

    let errors = errors.lock().expect("fixture lock");
    assert!(errors.iter().any(|error| error == "input:input boom"));
    assert!(errors
        .iter()
        .any(|error| error == "before_provider_headers:extension handler panicked: header panic"));
    assert!(errors.iter().any(|error| error == "context:context boom"));
}

#[test]
fn tool_result_and_before_agent_start_preserve_prior_patches() {
    let mut first = fixture_extension("first.ts");
    first.handlers.insert(
        "tool_result".to_string(),
        vec![handler(|_, _| {
            Ok(Some(json!({
                "content": [{"type": "text", "text": "first"}],
                "details": {"source": "first"},
            })))
        })],
    );
    first.handlers.insert(
        "before_agent_start".to_string(),
        vec![handler(|_, event| {
            Ok(Some(json!({
                "systemPrompt": format!("{}\nfirst", event["systemPrompt"].as_str().unwrap()),
                "message": {"customType": "fixture", "content": "first"},
            })))
        })],
    );

    let mut second = fixture_extension("second.ts");
    second.handlers.insert(
        "tool_result".to_string(),
        vec![handler(|_, _| Ok(Some(json!({"isError": true}))))],
    );
    second.handlers.insert(
        "before_agent_start".to_string(),
        vec![handler(|_, event| {
            Ok(Some(json!({
                "systemPrompt": format!("{}\nsecond", event["systemPrompt"].as_str().unwrap()),
            })))
        })],
    );

    let runner = runner_with(vec![first, second]);
    assert_eq!(
        runner.emit_tool_result(json!({
            "content": [{"type": "text", "text": "base"}],
            "details": {"source": "base"},
            "isError": false,
            "usage": {"input": 1}
        })),
        Some(json!({
            "content": [{"type": "text", "text": "first"}],
            "details": {"source": "first"},
            "isError": true,
            "usage": {"input": 1}
        }))
    );

    assert_eq!(
        runner.emit_before_agent_start("prompt", None, "base", &json!({"cwd": "/fixture"})),
        Some(json!({
            "messages": [{"customType": "fixture", "content": "first"}],
            "systemPrompt": "base\nfirst\nsecond"
        }))
    );
}

#[test]
fn command_errors_are_reported_without_panicking_the_runner() {
    let mut extension = fixture_extension("commands.ts");
    extension.commands.insert(
        "broken".to_string(),
        pi_coding_agent::core::extensions::types::RegisteredCommand {
            name: "broken".to_string(),
            source_info: extension.source_info.clone(),
            description: None,
            handler: handler(|_, _| Err("command failed".to_string())),
        },
    );
    let runner = runner_with(vec![extension]);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&errors);
    let _unsubscribe = runner.on_error(Arc::new(move |error| {
        received.lock().expect("fixture lock").push(error);
    }));

    assert_eq!(
        runner.execute_command("broken", "args"),
        Err("command failed".to_string())
    );
    let errors = errors.lock().expect("fixture lock");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].extension_path, "command:broken");
    assert_eq!(errors[0].event, "command");
}

#[test]
fn stale_runtime_invalidates_subscriptions_and_keeps_first_message() {
    let mut runtime = ExtensionRuntime::new();
    let unsubscribed = Arc::new(Mutex::new(0));
    let count = Arc::clone(&unsubscribed);
    let subscription = runtime.track_event_bus_subscription(Arc::new(move || {
        *count.lock().expect("fixture lock") += 1;
    }));
    assert!(runtime.assert_active().is_ok());
    runtime.invalidate(None);
    runtime.invalidate(Some("second message"));
    assert_eq!(*unsubscribed.lock().expect("fixture lock"), 1);
    assert_eq!(runtime.stale_error().as_deref(), Some(STALE_MESSAGE));
    assert!(runtime.assert_active().is_err());
    subscription();
}

#[test]
fn external_loader_isolates_one_failed_extension_and_keeps_order() {
    let root = std::env::temp_dir().join(format!("pi-extension-parity-{}", uuid::Uuid::new_v4()));
    let extension_dir = root.join("extensions");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&extension_dir).expect("extension dir");
    fs::create_dir_all(&bin_dir).expect("bin dir");
    fs::write(extension_dir.join("b-good.ts"), "fixture").expect("good extension");
    fs::write(extension_dir.join("a-bad.ts"), "fixture").expect("bad extension");
    let runner = bin_dir.join("runner");
    fs::write(
        &runner,
        "#!/bin/sh\ncase \"$1\" in *a-bad.ts) echo 'fixture failure' >&2; exit 7;; esac\n",
    )
    .expect("runner");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o755)).expect("runner mode");
    }

    let paths = discover_extensions_in_dir(&extension_dir);
    assert_eq!(
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        vec!["a-bad.ts", "b-good.ts"]
    );
    let result = load_extensions(
        &paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        &root.to_string_lossy(),
        None,
        Some(runner.to_str().unwrap()),
    );
    assert_eq!(result.extensions.len(), 1);
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].error.contains("fixture failure"));
    assert!(result.extensions[0].path.ends_with("b-good.ts"));

    let direct = run_external_extension(
        "b-good.ts",
        &extension_dir.join("b-good.ts"),
        Some(runner.to_str().unwrap()),
        Some(2_000),
    )
    .expect("good extension remains loadable");
    assert!(direct.source_info.base_dir.is_some());

    fs::remove_dir_all(root).expect("fixture cleanup");
}

#[test]
fn wrapper_matches_upstream_active_tool_invariant_and_deduplication() {
    let tool = pi_coding_agent::core::extensions::types::RegisteredTool {
        name: "fixture".to_string(),
        description: "fixture".to_string(),
        parameters: json!({}),
        source_info: SourceInfo::synthetic("fixture", "fixture", None),
        execute: None,
    };
    let wrapped = wrap_registered_tool(
        tool,
        Arc::new(|_| WrappedToolResult {
            added_tool_names: vec!["existing".to_string()],
            ..WrappedToolResult::default()
        }),
    );
    let result = (wrapped.execute)(WrappedToolCall {
        tool_call_id: "call".to_string(),
        params: json!({}),
        active_tools_before: vec!["bash".to_string(), "read".to_string()],
        active_tools_after: vec![
            "bash".to_string(),
            "read".to_string(),
            "existing".to_string(),
            "new".to_string(),
            "new".to_string(),
        ],
    });
    assert_eq!(result.added_tool_names, vec!["existing", "new"]);

    let removed = (wrapped.execute)(WrappedToolCall {
        tool_call_id: "call-2".to_string(),
        params: json!({}),
        active_tools_before: vec!["bash".to_string(), "read".to_string()],
        active_tools_after: vec!["bash".to_string(), "new".to_string()],
    });
    assert_eq!(removed.added_tool_names, vec!["existing"]);
}

#[test]
fn reserved_shortcut_resolution_remains_deterministic() {
    let mut extension = fixture_extension("shortcuts.ts");
    extension.shortcuts.insert(
        "Ctrl+C".to_string(),
        pi_coding_agent::core::extensions::types::ExtensionShortcut {
            shortcut: "Ctrl+C".to_string(),
            description: None,
            handler: handler(|_, _| Ok(None)),
            extension_path: "shortcuts.ts".to_string(),
        },
    );
    let mut runner = runner_with(vec![extension]);
    let shortcuts = runner.get_shortcuts(&KeybindingsConfig {
        bindings: BTreeMap::from([("app.interrupt".to_string(), vec!["ctrl+c".to_string()])]),
    });
    assert!(shortcuts.is_empty());
    assert_eq!(runner.get_shortcut_diagnostics().len(), 1);
}

#[test]
fn node_bridge_executes_async_factory_commands_hooks_renderers_and_provider_config() {
    let root = std::env::temp_dir().join(format!("pi-extension-bridge-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("bridge fixture dir");
    fs::write(root.join("package.json"), r#"{"type":"module"}"#).expect("module package");
    fs::write(root.join("helper.js"), "export const jsSuffix = '[js]';\n").expect("js helper");
    fs::write(root.join("helper.ts"), "export const tsSuffix = '[ts]';\n").expect("ts helper");
    let entry = root.join("bridge.ts");
    fs::write(
        &entry,
        r#"
import { jsSuffix } from "./helper.js";
import { tsSuffix } from "./helper.ts";

export default async function (pi) {
  await new Promise((resolve) => setTimeout(resolve, 2));
  pi.registerCommand("bridge", {
    description: "Bridge command",
    handler: async (args, ctx) => ({ args, mode: ctx.mode }),
  });
  pi.on("input", async (event) => ({
    action: "transform",
    text: `${event.text}[bridge]`,
  }));
  pi.on("before_provider_headers", async (event) => {
    await Promise.resolve();
    event.headers["X-Bridge"] = "yes";
  });
  pi.registerMessageRenderer("bridge-message", (message, options) => ({
    value: message.value,
    expanded: options.expanded,
  }));
  pi.registerEntryRenderer("bridge-entry", (entry, options) => ({
    id: entry.id,
    expanded: options.expanded,
  }));
  pi.registerMarkdownTransformer(async (markdown, context) =>
    `${markdown}:${context.messageType}:${context.availableWidth}`
  );
  pi.registerFlag("bridge-flag", { type: "boolean", default: true });
  pi.registerTool({
    name: "bridge-tool",
    description: "Bridge tool",
    parameters: { type: "object", properties: { value: { type: "number" } } },
    execute: async (toolCallId, params) => {
      const activeBefore = pi.getActiveTools();
      const allTools = pi.getAllTools();
      const commands = pi.getCommands();
      const sessionBefore = pi.getSessionName();
      pi.setActiveTools([...activeBefore, "bridge-added"]);
      pi.setSessionName("bridge session");
      pi.setLabel("entry-1", "bridge label");
      pi.appendEntry("bridge-entry", { value: params.value });
      pi.sendMessage({ customType: "bridge", content: { value: params.value } });
      pi.sendUserMessage("bridge user message");
      const modelSet = await pi.setModel({ id: "bridge-model" });
      const thinkingBefore = pi.getThinkingLevel();
      pi.setThinkingLevel("high");
      return {
        toolCallId,
        params,
        activeBefore,
        allTools,
        commands,
        sessionBefore,
        modelSet,
        thinkingBefore,
        imported: `${jsSuffix}${tsSuffix}`,
      };
    },
  });
  pi.registerProvider("bridge-provider", {
    name: "Bridge Provider",
    baseUrl: "https://bridge.invalid/v1",
    api: "openai-completions",
    models: [{
      id: "bridge-model",
      name: "Bridge Model",
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 4096,
      maxTokens: 256,
    }],
  });
}
"#,
    )
    .expect("bridge fixture");

    let paths = vec![entry.to_string_lossy().to_string()];
    let host = Arc::new(RecordingHost::default());
    *host.active_tools.lock().expect("initial active tools lock") = vec!["bash".into()];
    *host.thinking_level.lock().expect("initial thinking lock") = "medium".into();
    let result = load_extensions_with_host_actions(
        &paths,
        &root.to_string_lossy(),
        None,
        Some("node"),
        host.clone(),
    );
    assert!(
        result.errors.is_empty(),
        "bridge load errors: {:?}",
        result.errors
    );
    assert_eq!(result.extensions.len(), 1);
    let extension = &result.extensions[0];
    assert!(extension.commands.contains_key("bridge"));
    assert!(extension.handlers.contains_key("input"));
    assert!(extension.message_renderers.contains_key("bridge-message"));
    assert!(extension.entry_renderers.contains_key("bridge-entry"));
    assert!(extension.markdown_transformer.is_some());
    let queued_providers = result
        .runtime
        .lock()
        .expect("bridge runtime lock")
        .pending_provider_registrations
        .clone();
    assert_eq!(queued_providers.len(), 1);
    assert_eq!(queued_providers[0].name, "bridge-provider");
    assert_eq!(queued_providers[0].config["api"], "openai-completions");

    let runtime = Arc::clone(&result.runtime);
    let runner = ExtensionRunner::new(result.extensions, runtime, root.to_string_lossy().into());
    assert_eq!(
        runner
            .execute_command("bridge", "one two")
            .expect("bridge command"),
        Some(json!({"args": "one two", "mode": "print"}))
    );
    assert_eq!(
        runner.emit_input("hello", None, "interactive", None),
        InputEventResult::transform("hello[bridge]", None)
    );
    assert_eq!(
        runner.emit_before_provider_headers(json!({"User-Agent": "fixture/1"})),
        json!({"User-Agent": "fixture/1", "X-Bridge": "yes"})
    );
    assert_eq!(
        runner
            .render_message(
                "bridge-message",
                &json!({"value": 7}),
                &json!({"expanded": true}),
            )
            .expect("bridge message renderer"),
        Some(json!({"value": 7, "expanded": true}))
    );
    assert_eq!(
        runner
            .render_entry(
                "bridge-entry",
                &json!({"id": "entry-1"}),
                &json!({"expanded": false}),
            )
            .expect("bridge entry renderer"),
        Some(json!({"id": "entry-1", "expanded": false}))
    );
    assert_eq!(
        runner.apply_markdown_transformers(
            "body",
            &pi_coding_agent::core::extensions::types::MarkdownTransformContext {
                message_type: "assistant".to_string(),
                is_streaming: false,
                available_width: 78,
            },
        ),
        "body:assistant:78"
    );
    assert_eq!(
        runner.get_flag_values().get("bridge-flag"),
        Some(&json!(true))
    );
    let tool_result = runner
        .execute_tool("bridge-tool", "call-bridge", json!({"value": 7}))
        .expect("bridge tool execute");
    assert_eq!(tool_result["toolCallId"], "call-bridge");
    assert_eq!(tool_result["params"], json!({"value": 7}));
    assert_eq!(tool_result["activeBefore"], json!(["bash"]));
    assert_eq!(
        tool_result["allTools"],
        json!([{
            "name": "bridge-tool",
            "description": "bridge tool",
            "parameters": {},
        }])
    );
    assert_eq!(
        tool_result["commands"],
        json!([{
            "name": "bridge",
            "description": "Bridge command",
        }])
    );
    assert_eq!(tool_result["imported"], "[js][ts]");
    assert_eq!(tool_result["sessionBefore"], Value::Null);
    assert_eq!(tool_result["modelSet"], true);
    assert_eq!(tool_result["thinkingBefore"], "medium");
    assert_eq!(
        *host.active_tools.lock().expect("final active tools lock"),
        vec!["bash", "bridge-added"]
    );
    assert_eq!(
        *host.session_name.lock().expect("final session name lock"),
        Some("bridge session".to_string())
    );
    assert_eq!(
        *host.thinking_level.lock().expect("final thinking lock"),
        "high"
    );
    let actions = host
        .calls
        .lock()
        .expect("host calls lock")
        .iter()
        .map(|(action, _)| *action)
        .collect::<Vec<_>>();
    for action in [
        ExtensionHostAction::SetActiveTools,
        ExtensionHostAction::SetSessionName,
        ExtensionHostAction::SetLabel,
        ExtensionHostAction::AppendEntry,
        ExtensionHostAction::SendMessage,
        ExtensionHostAction::SendUserMessage,
        ExtensionHostAction::SetModel,
        ExtensionHostAction::SetThinkingLevel,
    ] {
        assert!(actions.contains(&action), "missing host action {action:?}");
    }
    runner.invalidate(None);
    let stale_error = runner
        .execute_command("bridge", "after-invalidation")
        .expect_err("stale external callbacks must be rejected");
    assert!(
        stale_error.contains("stale") || stale_error.contains("invalidated"),
        "stale error: {stale_error}"
    );
    let loaded_extension = runner
        .extensions()
        .first()
        .expect("loaded bridge extension");
    assert_eq!(loaded_extension.source_info.source, "local");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn node_bridge_handler_context_exposes_safe_snapshot_and_control_actions() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "pi-extension-bridge-context-handler-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).expect("context handler fixture dir");
    let entry = root.join("context-handler.ts");
    fs::write(
        &entry,
        r#"
export default function (pi) {
  pi.on("input", (event, ctx) => {
    const before = {
      model: ctx.model,
      scopedModels: ctx.scopedModels,
      idle: ctx.isIdle(),
      trusted: ctx.isProjectTrusted(),
      signal: ctx.signal?.aborted ?? null,
      pending: ctx.hasPendingMessages(),
      usage: ctx.getContextUsage(),
      prompt: ctx.getSystemPrompt(),
      options: ctx.getSystemPromptOptions(),
    };
    ctx.abort();
    ctx.compact({ customInstructions: "handler compact" });
    ctx.shutdown();
    return {
      action: "transform",
      text: JSON.stringify({ before, afterAborted: ctx.signal?.aborted ?? null }),
    };
  });
}
"#,
    )
    .expect("context handler fixture");

    let host = Arc::new(ExtensionHostState::default());
    host.set_model(Some(json!({"provider": "fixture", "id": "model-1"})));
    host.set_scoped_models(vec![json!({
        "model": {"provider": "fixture", "id": "scoped-1"},
        "thinkingLevel": "high",
    })]);
    host.set_idle(false);
    host.set_project_trusted(false);
    host.set_signal(Some(Arc::new(std::sync::atomic::AtomicBool::new(false))));
    host.set_has_pending_messages(true);
    host.set_context_usage(Some(json!({
        "tokens": 12,
        "contextWindow": 100,
        "percent": 0.12,
    })));
    host.set_system_prompt("fixture system prompt");
    host.set_system_prompt_options(json!({
        "cwd": root.to_string_lossy(),
        "selectedTools": ["bridge-tool"],
    }));

    let result = load_extensions_with_host_actions(
        &[entry.to_string_lossy().to_string()],
        &root.to_string_lossy(),
        None,
        Some("node"),
        host.clone(),
    );
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    let runtime = Arc::clone(&result.runtime);
    let runner = ExtensionRunner::new(result.extensions, runtime, root.to_string_lossy().into());
    let transformed = runner.emit_input("hello", None, "interactive", None);
    let payload: Value = serde_json::from_str(
        transformed
            .text
            .as_deref()
            .expect("handler transformed text"),
    )
    .expect("handler returned JSON snapshot");
    assert_eq!(
        payload["before"],
        json!({
            "model": {"provider": "fixture", "id": "model-1"},
            "scopedModels": [{
                "model": {"provider": "fixture", "id": "scoped-1"},
                "thinkingLevel": "high",
            }],
            "idle": false,
            "trusted": false,
            "signal": false,
            "pending": true,
            "usage": {"tokens": 12, "contextWindow": 100, "percent": 0.12},
            "prompt": "fixture system prompt",
            "options": {
                "cwd": root.to_string_lossy(),
                "selectedTools": ["bridge-tool"],
            },
        })
    );
    assert_eq!(payload["afterAborted"], true);
    assert!(host
        .snapshot()
        .get("signal")
        .and_then(Value::as_object)
        .and_then(|signal| signal.get("aborted"))
        .and_then(Value::as_bool)
        .unwrap_or(false));
    assert_eq!(
        host.drain_pending_actions(),
        vec![
            json!({"type": "abort"}),
            json!({"type": "compact", "options": {"customInstructions": "handler compact"}}),
            json!({"type": "shutdown"}),
        ]
    );
    runner.invalidate(Some("context handler test complete"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn node_bridge_isolates_async_handler_failures_and_keeps_process_alive() {
    let root = std::env::temp_dir().join(format!(
        "pi-extension-bridge-failure-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).expect("bridge failure fixture dir");
    let entry = root.join("failure.ts");
    fs::write(
        &entry,
        r#"
export default function (pi) {
  pi.on("input", async (event) => {
    await Promise.resolve();
    if (event.text === "boom") throw new Error("bridge handler boom");
    return { action: "transform", text: `${event.text}[ok]` };
  });
  pi.registerCommand("broken", {
    handler: async () => { throw new Error("bridge command boom"); },
  });
}
"#,
    )
    .expect("bridge failure fixture");

    let result = load_extensions(
        &[entry.to_string_lossy().to_string()],
        &root.to_string_lossy(),
        None,
        Some("node"),
    );
    assert!(
        result.errors.is_empty(),
        "bridge load errors: {:?}",
        result.errors
    );
    let errors = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&errors);
    let runner = ExtensionRunner::new(
        result.extensions,
        result.runtime,
        root.to_string_lossy().into(),
    );
    let _unsubscribe = runner.on_error(Arc::new(move |error| {
        received
            .lock()
            .expect("bridge error lock")
            .push(format!("{}:{}", error.event, error.error));
    }));

    assert_eq!(
        runner.emit_input("boom", None, "interactive", None),
        InputEventResult::continue_with("boom", None)
    );
    assert_eq!(
        runner.emit_input("after", None, "interactive", None),
        InputEventResult::transform("after[ok]", None)
    );
    assert_eq!(
        runner.execute_command("broken", "args"),
        Err("bridge command boom".to_string())
    );
    let errors = errors.lock().expect("bridge error lock");
    assert!(errors
        .iter()
        .any(|error| error.contains("input:bridge handler boom")));
    assert!(errors
        .iter()
        .any(|error| error.contains("command:bridge command boom")));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn node_bridge_timeout_terminates_persistent_child() {
    let root = std::env::temp_dir().join(format!(
        "pi-extension-bridge-timeout-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).expect("bridge timeout dir");
    let entry = root.join("timeout.ts");
    fs::write(
        &entry,
        r#"
export default function (pi) {
  pi.registerCommand("slow", {
    handler: async () => new Promise((resolve) => setTimeout(() => resolve({ ok: true }), 600)),
  });
}
"#,
    )
    .expect("bridge timeout fixture");

    let extension = run_external_extension(
        entry.to_string_lossy().as_ref(),
        &entry,
        Some("node"),
        Some(250),
    )
    .expect("timeout fixture should load");
    let runner = runner_with(vec![extension]);
    let error = runner
        .execute_command("slow", "args")
        .expect_err("slow callback must time out");
    assert!(error.contains("Extension bridge request timed out after 250ms"));
    let second_error = runner
        .execute_command("slow", "args")
        .expect_err("timed-out bridge must stay closed");
    assert!(second_error.contains("Extension bridge exited"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn node_bridge_keeps_host_actions_unbound_during_factory_load() {
    let root = std::env::temp_dir().join(format!(
        "pi-extension-bridge-prebind-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).expect("prebind fixture dir");
    let entry = root.join("prebind.ts");
    fs::write(
        &entry,
        r#"
export default async function (pi) {
  await pi.getSessionName();
}
"#,
    )
    .expect("prebind fixture");

    let result = load_extensions(
        &[entry.to_string_lossy().to_string()],
        &root.to_string_lossy(),
        None,
        Some("node"),
    );
    assert!(result.extensions.is_empty());
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0]
            .error
            .contains("Extension runtime not initialized"),
        "pre-bind error: {:?}",
        result.errors
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn node_bridge_reports_invalid_exports_factory_failures_and_native_provider_boundary() {
    let root = std::env::temp_dir().join(format!(
        "pi-extension-bridge-errors-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).expect("bridge error fixture dir");
    let no_default = root.join("no-default.ts");
    let throws = root.join("throws.ts");
    let native_provider = root.join("native-provider.ts");
    fs::write(&no_default, "export const notDefault = 1;\n").expect("no-default fixture");
    fs::write(
        &throws,
        "export default async function () { await Promise.resolve(); throw new Error('init boom'); }\n",
    )
    .expect("throw fixture");
    fs::write(
        &native_provider,
        "export default function (pi) { pi.registerProvider({ id: 'native', stream: () => {} }); }\n",
    )
    .expect("native provider fixture");

    let paths = vec![
        no_default.to_string_lossy().to_string(),
        throws.to_string_lossy().to_string(),
        native_provider.to_string_lossy().to_string(),
    ];
    let result = load_extensions(&paths, &root.to_string_lossy(), None, Some("node"));
    assert!(result.extensions.is_empty());
    assert_eq!(result.errors.len(), 3);
    assert!(result.errors.iter().any(|error| {
        error.path.ends_with("no-default.ts")
            && error
                .error
                .contains("does not export a valid factory function")
    }));
    assert!(result
        .errors
        .iter()
        .any(|error| { error.path.ends_with("throws.ts") && error.error.contains("init boom") }));
    assert!(result.errors.iter().any(|error| {
        error.path.ends_with("native-provider.ts")
            && error
                .error
                .contains("does not support native provider callbacks")
    }));
    let _ = fs::remove_dir_all(root);
}
