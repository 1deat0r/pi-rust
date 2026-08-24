use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use pi_coding_agent::core::extensions::loader::{
    create_extension_runtime, discover_extensions_in_dir, load_extension_from_factory,
    load_extensions, run_external_extension,
};
use pi_coding_agent::core::extensions::runner::{
    ExtensionRunner, InputEventResult, KeybindingsConfig,
};
use pi_coding_agent::core::extensions::types::{
    Extension, ExtensionContext, ExtensionRuntime, HandlerFn, InputAction, SourceInfo,
    STALE_MESSAGE,
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
