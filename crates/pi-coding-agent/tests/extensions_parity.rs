#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Rust-native extension-loader parity tests.
//!
//! Filesystem module execution is intentionally outside the product contract.
//! These tests exercise the supported factory API and verify that discovery
//! cannot turn a source path into a successful load.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_coding_agent::core::extensions::integration::ExtensionHostState;
use pi_coding_agent::core::extensions::loader::{
    create_extension_runtime, discover_and_load_extensions, discover_extensions_in_dir,
    load_extension_from_factory, load_extensions, resolve_extension_entries,
    RUST_NATIVE_ONLY_ERROR,
};
use pi_coding_agent::core::extensions::runner::ExtensionRunner;
use pi_coding_agent::core::extensions::types::{
    Extension, ExtensionContext, ExtensionHostAction, ExtensionHostActionOutcome,
    ExtensionHostActions, FlagType, HandlerFn, RegisteredTool, SourceInfo, ToolExecutionRequest,
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
        source_info: SourceInfo::synthetic(path, "rust-native", None),
        ..Extension::default()
    }
}

#[derive(Default)]
struct RecordingHost {
    calls: Mutex<Vec<(ExtensionHostAction, Value)>>,
}

impl ExtensionHostActions for RecordingHost {
    fn dispatch(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String> {
        self.calls
            .lock()
            .expect("host calls lock")
            .push((action, args.clone()));
        Ok(Value::Null)
    }
}

fn sandbox(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pi-rust-native-extension-parity-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&dir).expect("create parity sandbox");
    dir
}

#[test]
fn rust_factory_registration_and_runner_dispatch_are_native() {
    let runtime = create_extension_runtime();
    let extension = load_extension_from_factory(
        |api| {
            api.on(
                "input",
                handler(|_, event| {
                    Ok(Some(json!({
                        "action": "transform",
                        "text": format!(
                            "{}[rust]",
                            event["text"].as_str().unwrap_or_default()
                        ),
                    })))
                }),
            )?;
            api.register_command(
                "echo",
                Some("Echo fixture arguments".to_string()),
                handler(|_, event| Ok(Some(json!({"args": event["args"]})))),
            )?;
            api.register_tool(RegisteredTool {
                name: "native-tool".to_string(),
                label: "Native tool".to_string(),
                description: "Rust-native tool".to_string(),
                parameters: json!({"type": "object"}),
                source_info: SourceInfo::synthetic("<inline:parity>", "inline", None),
                execute: Some(Arc::new(|request: ToolExecutionRequest| {
                    Ok(json!({
                        "callId": request.tool_call_id,
                        "params": request.params,
                    }))
                })),
                ..Default::default()
            })?;
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
            api.register_flag(
                "native-flag",
                Some("Native flag".to_string()),
                FlagType::String,
                Some(json!("default")),
            )?;
            api.register_provider("native-config", json!({"api": "fixture"}))?;
            api.register_native_provider("native-provider")?;
            Ok(())
        },
        "/fixture/project",
        Arc::clone(&runtime),
        "<inline:parity>",
    )
    .expect("Rust factory must load");

    assert!(extension.handlers.contains_key("input"));
    assert!(extension.commands.contains_key("echo"));
    assert!(extension.tools.contains_key("native-tool"));
    assert!(extension.message_renderers.contains_key("fixture"));
    assert!(extension.entry_renderers.contains_key("fixture-entry"));
    assert!(extension.markdown_transformer.is_some());
    let runtime_guard = runtime.lock().expect("runtime lock");
    assert_eq!(runtime_guard.flag_values["native-flag"], "default");
    assert_eq!(
        runtime_guard.pending_provider_registrations[0].name,
        "native-config"
    );
    assert_eq!(
        runtime_guard.pending_native_provider_registrations[0].provider,
        "native-provider"
    );
    drop(runtime_guard);

    let runner = ExtensionRunner::new(
        vec![extension],
        Arc::clone(&runtime),
        "/fixture/project".to_string(),
    );
    assert_eq!(
        runner
            .execute_command("echo", "one two")
            .expect("command dispatch"),
        Some(json!({"args": "one two"}))
    );
    assert_eq!(
        runner
            .emit_input("hello", None, "print", None)
            .text
            .as_deref(),
        Some("hello[rust]")
    );
    assert_eq!(
        runner
            .execute_tool("native-tool", "call-1", json!({"value": 7}))
            .expect("tool dispatch"),
        json!({"callId": "call-1", "params": {"value": 7}})
    );
    assert_eq!(
        runner
            .render_message("fixture", &json!({"value": 7}), &json!({}))
            .expect("message renderer"),
        Some(json!({"message": {"value": 7}}))
    );
    assert_eq!(
        runner
            .render_entry("fixture-entry", &json!({"id": 1}), &json!({}))
            .expect("entry renderer"),
        Some(json!({"entry": {"id": 1}}))
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
fn rust_factory_failures_are_reported_without_a_loaded_extension() {
    let error = load_extension_from_factory(
        |_api| Err("factory failed".to_string()),
        "/fixture/project",
        create_extension_runtime(),
        "<inline:error>",
    )
    .expect_err("factory error");
    assert!(error.error.contains("Failed to load Rust extension"));
    assert!(error.error.contains("factory failed"));
}

#[test]
fn filesystem_paths_are_rejected_without_a_runner_or_silent_success() {
    let paths = vec!["/tmp/example.ts".to_string(), "/tmp/example.js".to_string()];
    let result = load_extensions(&paths, "/tmp", None, Some("must-not-run"));
    assert!(result.extensions.is_empty());
    assert_eq!(result.errors.len(), paths.len());
    for (error, path) in result.errors.iter().zip(paths) {
        assert_eq!(error.path, path);
        assert!(error.error.contains(RUST_NATIVE_ONLY_ERROR));
        assert!(error.error.contains("load_extension_from_factory"));
    }
}

#[test]
fn automatic_discovery_reports_source_paths_without_executing_them() {
    let root = sandbox("discovery");
    let local = root.join(".pi/extensions");
    let nested = local.join("nested");
    fs::create_dir_all(&nested).expect("create local extension directories");
    fs::write(local.join("direct.ts"), "export default () => {}").expect("write source");
    fs::write(nested.join("index.js"), "export default () => {}").expect("write source");
    fs::write(local.join("helper.tsx"), "export default () => {}").expect("write ignored source");

    assert_eq!(
        discover_extensions_in_dir(&local),
        vec![local.join("direct.ts"), nested.join("index.js")]
    );
    let result = discover_and_load_extensions(
        &[],
        &root.to_string_lossy(),
        &root.to_string_lossy(),
        None,
        None,
    );
    assert!(result.extensions.is_empty());
    assert_eq!(result.errors.len(), 2);
    assert!(result
        .errors
        .iter()
        .all(|error| error.error.contains(RUST_NATIVE_ONLY_ERROR)));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolver_keeps_manifest_path_resolution_for_non_source_metadata() {
    let root = sandbox("resolver");
    fs::write(
        root.join("package.json"),
        r#"{ "pi": { "extensions": ["entry.rust"] } }"#,
    )
    .expect("write package manifest");
    fs::write(root.join("entry.rust"), "metadata-only path").expect("write entry");
    assert_eq!(
        resolve_extension_entries(&root),
        Some(vec![root.join("entry.rust")])
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn host_binding_is_available_for_native_factories() {
    let host = Arc::new(RecordingHost::default());
    let result = pi_coding_agent::core::extensions::loader::load_extensions_with_host_actions(
        &[],
        "/fixture/project",
        None,
        None,
        host,
    );
    assert!(result
        .runtime
        .lock()
        .expect("runtime lock")
        .is_initialized());
}

#[test]
fn native_handler_can_call_the_live_extension_context_host_surface() {
    let runtime = create_extension_runtime();
    let extension = load_extension_from_factory(
        |api| {
            api.register_tool(RegisteredTool {
                name: "host-surface".to_string(),
                label: "Host surface".to_string(),
                description: "host surface fixture".to_string(),
                parameters: json!({"type": "object"}),
                source_info: SourceInfo::synthetic("<inline:host-surface>", "inline", None),
                execute: Some(Arc::new(|request: ToolExecutionRequest| {
                    let context = &request.context;
                    context.wait_for_idle()?;
                    let session_name = context.get_session_name()?;
                    let model = context.get_model()?;
                    let scoped_models = context.get_scoped_models()?;
                    let active_tools = context.get_active_tools()?;
                    let all_tools = context.get_all_tools()?;
                    let commands = context.get_commands()?;
                    let thinking_level = context.get_thinking_level()?;
                    let is_idle = context.is_idle()?;
                    let trusted = context.is_project_trusted()?;
                    let signal = context.signal()?;
                    let pending_messages = context.has_pending_messages()?;
                    let usage = context.get_context_usage()?;
                    let system_prompt = context.get_system_prompt()?;
                    let system_prompt_options = context.get_system_prompt_options()?;
                    context.set_session_name(Some("renamed"))?;
                    context.set_label("entry-1", Some("bookmark"))?;
                    context.set_thinking_level("high")?;
                    context.set_active_tools(&["tool-b".to_string()])?;
                    let send = context.send_message(json!({"customType": "fixture"}), None)?;
                    let send_user = context.send_user_message(json!("hello"), None)?;
                    let append = context.append_entry("fixture", Some(json!({"value": 1})))?;
                    let model_change = context.set_model(json!({"id": "model-b"}))?;
                    let compact = context.compact(Some(json!({"reserveTokens": 10})))?;
                    let abort = context.abort()?;
                    context.tool_update(json!({"text": "partial"}))?;
                    let signal_after_abort = context.signal()?;
                    let shutdown = context.shutdown()?;
                    let lifecycle = context.new_session(None)?;
                    let _fork = context.fork(Some("entry-1"), None)?;
                    let _navigate = context.navigate_tree(Some("leaf-1"), None)?;
                    let _switch = context.switch_session(Some("/fixture/session.jsonl"), None)?;
                    let _reload = context.reload(None)?;
                    let session_handle_name = context.session_manager()?.get_session_name()?;
                    let registry_tool_count = context.model_registry()?.get_all_tools()?.len();
                    Ok(json!({
                        "sessionName": session_name,
                        "model": model,
                        "scopedModels": scoped_models,
                        "activeTools": active_tools,
                        "allTools": all_tools,
                        "commands": commands,
                        "thinkingLevel": thinking_level,
                        "isIdle": is_idle,
                        "trusted": trusted,
                        "signal": signal,
                        "pendingMessages": pending_messages,
                        "usage": usage,
                        "systemPrompt": system_prompt,
                        "systemPromptOptions": system_prompt_options,
                        "send": matches!(send, ExtensionHostActionOutcome::Completed(Value::Null)),
                        "sendUser": matches!(send_user, ExtensionHostActionOutcome::Completed(Value::Null)),
                        "append": matches!(append, ExtensionHostActionOutcome::Completed(Value::Null)),
                        "modelChangePending": matches!(model_change, ExtensionHostActionOutcome::Pending(_)),
                        "compactAccepted": matches!(compact, ExtensionHostActionOutcome::Completed(Value::Null)),
                        "abort": matches!(abort, ExtensionHostActionOutcome::Completed(Value::Null)),
                        "signalAfterAbort": signal_after_abort,
                        "shutdownAccepted": matches!(shutdown, ExtensionHostActionOutcome::Completed(Value::Null)),
                        "lifecyclePending": matches!(lifecycle, ExtensionHostActionOutcome::Pending(_)),
                        "sessionHandleName": session_handle_name,
                        "registryToolCount": registry_tool_count,
                    }))
                })),
                ..Default::default()
            })?;
            Ok(())
        },
        "/fixture/project",
        Arc::clone(&runtime),
        "<inline:host-surface>",
    )
    .expect("host-surface fixture");

    let host = Arc::new(ExtensionHostState::new(
        Some("initial".to_string()),
        "medium",
    ));
    host.set_catalog(
        vec!["tool-a".to_string()],
        vec![json!({"name": "tool-a"})],
        vec![json!({"name": "command-a"})],
    );
    host.set_model(Some(json!({"id": "model-a"})));
    host.set_scoped_models(vec![json!({"id": "model-a"})]);
    host.set_idle(true);
    host.set_project_trusted(true);
    host.set_has_pending_messages(true);
    host.set_context_usage(Some(json!({"tokens": 12})));
    host.set_system_prompt("system");
    host.set_system_prompt_options(json!({"cwd": "/fixture/project"}));
    let signal = Arc::new(AtomicBool::new(false));
    host.set_signal(Some(signal.clone()));

    let mut runner = ExtensionRunner::new(
        vec![extension],
        Arc::clone(&runtime),
        "/fixture/project".to_string(),
    );
    runner.set_ui_context("rpc", true);
    runner.bind_core_with_actions(host.clone());
    let result = runner
        .execute_tool("host-surface", "host-call", json!({}))
        .expect("host surface tool");

    assert_eq!(result["sessionName"], "initial");
    assert_eq!(result["model"], json!({"id": "model-a"}));
    assert_eq!(result["scopedModels"], json!([{"id": "model-a"}]));
    assert_eq!(result["activeTools"], json!(["tool-a"]));
    assert_eq!(result["allTools"], json!([{"name": "tool-a"}]));
    assert_eq!(result["commands"], json!([{"name": "command-a"}]));
    assert_eq!(result["thinkingLevel"], "medium");
    assert_eq!(result["isIdle"], true);
    assert_eq!(result["trusted"], true);
    assert_eq!(result["signal"], json!({"aborted": false}));
    assert_eq!(result["pendingMessages"], true);
    assert_eq!(result["usage"], json!({"tokens": 12}));
    assert_eq!(result["systemPrompt"], "system");
    assert_eq!(
        result["systemPromptOptions"],
        json!({"cwd": "/fixture/project"})
    );
    assert_eq!(result["send"], true);
    assert_eq!(result["sendUser"], true);
    assert_eq!(result["append"], true);
    assert_eq!(result["modelChangePending"], true);
    assert_eq!(result["compactAccepted"], true);
    assert_eq!(result["abort"], true);
    assert_eq!(result["signalAfterAbort"], json!({"aborted": true}));
    assert_eq!(result["shutdownAccepted"], true);
    assert_eq!(result["lifecyclePending"], true);
    assert_eq!(result["sessionHandleName"], "renamed");
    assert_eq!(result["registryToolCount"], 1);
    assert!(signal.load(Ordering::Acquire));
    assert_eq!(host.drain_pending_messages().len(), 2);
    assert_eq!(host.drain_pending_entries().len(), 1);
    assert_eq!(host.drain_pending_lifecycle_actions().len(), 5);
    assert_eq!(host.drain_pending_actions().len(), 3);
    assert_eq!(
        host.requested_active_tools(),
        Some(vec!["tool-b".to_string()])
    );
}

#[test]
fn captured_native_context_rejects_host_calls_after_runtime_invalidation() {
    let runtime = create_extension_runtime();
    let host = Arc::new(ExtensionHostState::new(None, "medium"));
    let mut runner = ExtensionRunner::new(
        Vec::new(),
        Arc::clone(&runtime),
        "/fixture/project".to_string(),
    );
    runner.set_ui_context("rpc", true);
    runner.bind_core_with_actions(host);
    let context = runner.create_context_with_ui(true);
    assert!(context.host.is_bound());
    runner.invalidate(Some("replacement"));
    assert!(!context.host.is_bound());
    let error = context
        .get_session_name()
        .expect_err("stale native contexts must reject host calls");
    assert!(error.contains("replacement"));
    let ui_error = context
        .ui
        .notify("stale", None)
        .expect_err("stale native UI contexts must reject fire-and-forget calls");
    assert!(ui_error.contains("replacement"));
}

#[test]
fn native_fixture_helper_preserves_source_metadata_shape() {
    let extension = fixture_extension("<inline:fixture>");
    assert_eq!(extension.source_info.source, "rust-native");
    assert_eq!(extension.source_info.scope, "temporary");
}
