//! Rust-native extension-loader parity tests.
//!
//! Filesystem module execution is intentionally outside the product contract.
//! These tests exercise the supported factory API and verify that discovery
//! cannot turn a source path into a successful load.

use std::fs;
use std::sync::{Arc, Mutex};

use pi_coding_agent::core::extensions::loader::{
    create_extension_runtime, discover_and_load_extensions, discover_extensions_in_dir,
    load_extension_from_factory, load_extensions, resolve_extension_entries,
    RUST_NATIVE_ONLY_ERROR,
};
use pi_coding_agent::core::extensions::runner::ExtensionRunner;
use pi_coding_agent::core::extensions::types::{
    Extension, ExtensionContext, ExtensionHostAction, ExtensionHostActions, FlagType, HandlerFn,
    RegisteredTool, SourceInfo, ToolExecutionRequest,
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
                description: "Rust-native tool".to_string(),
                parameters: json!({"type": "object"}),
                source_info: SourceInfo::synthetic("<inline:parity>", "inline", None),
                execute: Some(Arc::new(|request: ToolExecutionRequest| {
                    Ok(json!({
                        "callId": request.tool_call_id,
                        "params": request.params,
                    }))
                })),
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
fn automatic_discovery_does_not_select_source_paths() {
    let root = sandbox("discovery");
    let local = root.join(".pi/extensions");
    fs::create_dir_all(&local).expect("create local extension directory");

    assert!(discover_extensions_in_dir(&local).is_empty());
    let result = discover_and_load_extensions(
        &[],
        &root.to_string_lossy(),
        &root.to_string_lossy(),
        None,
        None,
    );
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
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
fn native_fixture_helper_preserves_source_metadata_shape() {
    let extension = fixture_extension("<inline:fixture>");
    assert_eq!(extension.source_info.source, "rust-native");
    assert_eq!(extension.source_info.scope, "temporary");
}
