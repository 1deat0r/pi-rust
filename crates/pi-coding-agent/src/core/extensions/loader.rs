//! Rust-native extension loader.
//!
//! Extension discovery and path normalization remain available so callers can
//! inspect the same project, global, and package locations as the upstream
//! application.  The product no longer evaluates filesystem extension
//! modules.  A filesystem entry is therefore reported as a deterministic
//! Rust-native-only load error; executable extensions must be registered by a
//! Rust factory through [`load_extension_from_factory`].

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::CONFIG_DIR_NAME;
use crate::core::extensions::types::{
    EntryRenderer, Extension, ExtensionFlag, ExtensionHostActions, ExtensionLoadError,
    ExtensionRuntime, ExtensionShortcut, FlagType, HandlerFn, LoadExtensionsResult,
    MarkdownTransformer, MessageRenderer, PendingNativeProviderRegistration,
    PendingProviderRegistration, RegisteredCommand, RegisteredTool, RegistrationKind, SourceInfo,
};
use crate::core::pi_manifest::read_pi_manifest;

/// Public error marker used by callers and diagnostics to identify the
/// intentional zero-JS policy.
pub const RUST_NATIVE_ONLY_ERROR: &str = "Rust-native-only extension loader";

/// Rust-native equivalent of upstream `ExtensionAPI`.
///
/// Factories are evaluated synchronously by Rust and can register the same
/// callback, tool, renderer, flag, and provider surfaces used by the runner.
/// Runtime actions remain unavailable until the host binds the shared runtime.
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

    /// Queue a JSON provider configuration for the native provider registry.
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

    /// Queue a Rust-native provider identifier. Provider callback closures
    /// are registered by Rust factories through the native provider APIs.
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

/// Resolve non-source extension metadata entries from a directory.
///
/// JavaScript/TypeScript manifest entries and their conventional index files
/// are deliberately excluded. There is no executable filesystem extension
/// ABI in the zero-JS build; native extensions are registered by Rust factory
/// at compile time.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json = dir.join("package.json");
    if package_json.exists() {
        if let Some(manifest) = read_pi_manifest(&package_json) {
            if !manifest.extensions.is_empty() {
                let mut entries = Vec::new();
                for ext_path in &manifest.extensions {
                    let resolved = dir.join(ext_path);
                    if resolved.exists() && !is_source_extension_path(&resolved) {
                        entries.push(resolved);
                    }
                }
                if !entries.is_empty() {
                    return Some(entries);
                }
            }
        }
    }
    None
}

fn is_source_extension_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx")
    )
}

/// Discover extension paths for the legacy filesystem layout.
///
/// There are no dynamically loadable filesystem Rust factories, so automatic
/// discovery intentionally returns no executable entries. The resolver above
/// remains available for package metadata/path inspection, while explicit
/// paths still receive the deterministic zero-JS error from `load_extensions`.
pub fn discover_extensions_in_dir(_dir: &Path) -> Vec<PathBuf> {
    Vec::new()
}

fn filesystem_extension_error(path: &str) -> String {
    format!(
        "{RUST_NATIVE_ONLY_ERROR}: filesystem extension path {path:?} is not executable; register a Rust factory with load_extension_from_factory"
    )
}

fn make_extension(extension_path: &str, resolved_path: &Path) -> Extension {
    let source = if extension_path.starts_with('<') && extension_path.ends_with('>') {
        extract_synthetic_source(extension_path)
    } else {
        "rust-native".to_string()
    };
    let base_dir = if extension_path.starts_with('<') {
        None
    } else {
        resolved_path
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
    };
    Extension {
        path: extension_path.to_string(),
        resolved_path: resolved_path.to_string_lossy().into_owned(),
        hidden: false,
        source_info: SourceInfo::synthetic(extension_path, &source, base_dir),
        ..Default::default()
    }
}

/// Load a Rust-native extension factory. Factory panics and registration
/// errors are converted to the public load error shape.
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
            error: format!("Failed to load Rust extension: {error}"),
        }),
        Err(payload) => Err(ExtensionLoadError {
            path: extension_path.to_string(),
            error: format!(
                "Failed to load Rust extension: extension factory panicked: {}",
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

/// Load one filesystem path. The `runner` parameter is retained for source
/// compatibility but is intentionally ignored under the zero-JS policy.
pub fn load_extension(
    extension_path: &str,
    _cwd: &str,
    _runner: Option<&str>,
) -> Result<Extension, ExtensionLoadError> {
    Err(ExtensionLoadError {
        path: extension_path.to_string(),
        error: filesystem_extension_error(extension_path),
    })
}

fn normalize_extension_path(input: &str, normalize_unicode_spaces: bool) -> PathBuf {
    let mut normalized = input.to_string();
    if normalize_unicode_spaces {
        normalized = normalized
            .chars()
            .map(|character| match character {
                '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
                character => character,
            })
            .collect();
    }
    if normalized == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(suffix) = normalized.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(suffix);
        }
    }
    if normalized.starts_with("file://") {
        if let Ok(url) = url::Url::parse(&normalized) {
            if let Ok(path) = url.to_file_path() {
                return path;
            }
        }
    }
    PathBuf::from(normalized)
}

fn lexical_absolute_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let _ = normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// Resolve a path with tilde, file-URL, Unicode-space, and lexical
/// normalization behavior matching the existing loader contract.
pub fn resolve_relative_path(path: &str, base: &str) -> PathBuf {
    let path_buf = normalize_extension_path(path, true);
    let base_buf = normalize_extension_path(base, false);
    let candidate = if path_buf.is_absolute() {
        path_buf
    } else {
        base_buf.join(path_buf)
    };
    lexical_absolute_path(candidate)
}

/// Create the shared runtime used by Rust-native extension factories.
pub fn create_extension_runtime() -> Arc<Mutex<ExtensionRuntime>> {
    Arc::new(Mutex::new(ExtensionRuntime::new()))
}

/// Load explicit filesystem paths. Every path is rejected deterministically;
/// no executable source is inferred from a path or package manifest.
pub fn load_extensions(
    paths: &[String],
    _cwd: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    _runner: Option<&str>,
) -> LoadExtensionsResult {
    let runtime = runtime.unwrap_or_else(create_extension_runtime);
    let errors = paths
        .iter()
        .map(|path| ExtensionLoadError {
            path: path.clone(),
            error: filesystem_extension_error(path),
        })
        .collect();
    LoadExtensionsResult {
        extensions: Vec::new(),
        errors,
        runtime,
    }
}

/// Bind host actions to a Rust-native runtime.
pub fn load_extensions_with_host_actions(
    paths: &[String],
    cwd: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
    actions: Arc<dyn ExtensionHostActions>,
) -> LoadExtensionsResult {
    let result = load_extensions(paths, cwd, runtime, runner);
    result.bind_core_with_actions(actions);
    result
}

fn add_discovered_paths(
    cwd: &str,
    seen: &mut BTreeSet<PathBuf>,
    all_paths: &mut Vec<String>,
    paths: impl IntoIterator<Item = PathBuf>,
) {
    for path in paths {
        let absolute = if path.is_absolute() {
            path
        } else {
            Path::new(cwd).join(path)
        };
        if seen.insert(absolute.clone()) {
            all_paths.push(absolute.to_string_lossy().into_owned());
        }
    }
}

/// Discover standard project/global locations and explicit configured paths.
/// Automatic locations are now empty because no filesystem module is
/// executable; explicit configured paths are retained as errors so users get
/// a clear migration diagnostic rather than a silent success.
pub fn discover_and_load_extensions(
    configured_paths: &[String],
    cwd: &str,
    agent_dir: &str,
    runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    runner: Option<&str>,
) -> LoadExtensionsResult {
    let mut all_paths = Vec::new();
    let mut rejected_auto_paths = Vec::new();
    let mut seen = BTreeSet::new();

    let local_ext_dir = Path::new(cwd).join(CONFIG_DIR_NAME).join("extensions");
    add_discovered_paths(
        cwd,
        &mut seen,
        &mut all_paths,
        discover_extensions_in_dir(&local_ext_dir),
    );

    let global_ext_dir = Path::new(agent_dir).join("extensions");
    add_discovered_paths(
        cwd,
        &mut seen,
        &mut all_paths,
        discover_extensions_in_dir(&global_ext_dir),
    );

    for path in configured_paths {
        let resolved = resolve_relative_path(path, cwd);
        if resolved.is_dir() {
            // Keep resolver semantics available for direct callers, but do
            // not turn a package's unsupported source entry into an automatic
            // load attempt. Report explicitly configured unsupported entries
            // after the rest of the path collection is complete.
            for entry in resolve_extension_entries(&resolved).unwrap_or_default() {
                if seen.insert(entry.clone()) {
                    rejected_auto_paths.push(entry);
                }
            }
            continue;
        }
        // An explicitly configured file is intentionally surfaced as an
        // error, making the zero-JS migration actionable.
        add_discovered_paths(cwd, &mut seen, &mut all_paths, [resolved]);
    }

    let mut result = load_extensions(&all_paths, cwd, runtime, runner);
    result
        .errors
        .extend(rejected_auto_paths.into_iter().map(|path| {
            let path = path.to_string_lossy().into_owned();
            ExtensionLoadError {
                path: path.clone(),
                error: filesystem_extension_error(&path),
            }
        }));
    result
}

/// Preserve the bundled-loader API while requiring callers to migrate the
/// bundled implementation to a Rust factory.
pub fn load_bundled_extension(
    extension_path: &str,
    _runner: Option<&str>,
) -> Result<Extension, ExtensionLoadError> {
    Err(ExtensionLoadError {
        path: extension_path.to_string(),
        error: format!(
            "{RUST_NATIVE_ONLY_ERROR}: bundled path {extension_path:?} has no Rust factory; use load_extension_from_factory"
        ),
    })
}

/// Queue a provider registration while the runtime is active.
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

/// Queue a native provider registration while the runtime is active.
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

/// Drain queued provider registrations for the mode-level registry adapter.
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

/// Apply registered flag defaults without overwriting explicit values.
pub fn apply_flag_defaults(runtime: &Arc<Mutex<ExtensionRuntime>>, extensions: &[Extension]) {
    let Ok(mut guard) = runtime.lock() else {
        return;
    };
    for extension in extensions {
        for (name, flag) in &extension.flags {
            if let Some(default) = &flag.default {
                guard
                    .flag_values
                    .entry(name.clone())
                    .or_insert_with(|| default.clone());
            }
        }
    }
}

fn extract_synthetic_source(extension_path: &str) -> String {
    let inner = &extension_path[1..extension_path.len() - 1];
    inner.split(':').next().unwrap_or("temporary").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::runner::ExtensionRunner;
    use crate::core::extensions::types::{
        ExtensionContext, ExtensionHostAction, ExtensionHostActions, RegisteredTool,
        ToolExecutionRequest,
    };
    use serde_json::json;
    use std::fs;

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-rust-native-loader-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create loader sandbox");
        dir
    }

    #[derive(Default)]
    struct TestHost;

    impl ExtensionHostActions for TestHost {
        fn dispatch(
            &self,
            _action: ExtensionHostAction,
            _args: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    #[test]
    fn resolve_entries_preserves_manifest_paths() {
        let dir = sandbox("entries");
        fs::write(
            dir.join("package.json"),
            r#"{ "pi": { "extensions": ["main.rust"] } }"#,
        )
        .expect("write package manifest");
        fs::write(dir.join("main.rust"), "metadata-only path").expect("write manifest entry");
        let entries = resolve_extension_entries(&dir).expect("manifest entry");
        assert_eq!(entries, vec![dir.join("main.rust")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_entries_ignores_source_language_manifest_paths() {
        let dir = sandbox("source-entries");
        fs::write(
            dir.join("package.json"),
            r#"{ "pi": { "extensions": ["main.ts", "other.js", "native.rust"] } }"#,
        )
        .expect("write package manifest");
        fs::write(dir.join("main.ts"), "source").expect("write source entry");
        fs::write(dir.join("other.js"), "source").expect("write source entry");
        fs::write(dir.join("native.rust"), "metadata").expect("write native entry");
        let entries = resolve_extension_entries(&dir).expect("native manifest entry");
        assert_eq!(entries, vec![dir.join("native.rust")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn automatic_discovery_never_selects_unsupported_source_files() {
        let dir = sandbox("discovery");
        assert!(discover_extensions_in_dir(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_filesystem_load_is_deterministic_and_never_executes_a_runner() {
        let path = PathBuf::from("/tmp/extension.ts");
        let result = load_extension(path.to_string_lossy().as_ref(), ".", Some("ignored"));
        let error = result.expect_err("filesystem entry must be rejected");
        assert_eq!(error.path, path.to_string_lossy());
        assert!(error.error.contains(RUST_NATIVE_ONLY_ERROR));
        assert!(error.error.contains("load_extension_from_factory"));
    }

    #[test]
    fn rust_factory_registers_and_runs_native_surfaces() {
        let runtime = create_extension_runtime();
        let extension = load_extension_from_factory(
            |api| {
                api.on(
                    "input",
                    Arc::new(|_, event| {
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
                    Some("Rust command".to_string()),
                    Arc::new(|_, event| Ok(Some(json!({"args": event["args"]})))),
                )?;
                api.register_tool(RegisteredTool {
                    name: "native-tool".to_string(),
                    description: "Rust tool".to_string(),
                    parameters: json!({"type": "object"}),
                    source_info: SourceInfo::synthetic("<inline:rust>", "inline", None),
                    execute: Some(Arc::new(|request: ToolExecutionRequest| {
                        Ok(json!({"id": request.tool_call_id, "params": request.params}))
                    })),
                })?;
                api.register_flag(
                    "native-flag",
                    None,
                    FlagType::String,
                    Some(json!("default")),
                )?;
                api.register_provider("native-config", json!({"api": "fixture"}))?;
                api.register_native_provider("native-provider")?;
                Ok(())
            },
            "/fixture/project",
            Arc::clone(&runtime),
            "<inline:rust>",
        )
        .expect("Rust factory must load");

        assert!(extension.handlers.contains_key("input"));
        assert!(extension.commands.contains_key("echo"));
        assert!(extension.tools.contains_key("native-tool"));
        assert_eq!(
            runtime.lock().unwrap().flag_values["native-flag"],
            "default"
        );
        assert_eq!(
            runtime.lock().unwrap().pending_provider_registrations[0].name,
            "native-config"
        );
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .pending_native_provider_registrations[0]
                .provider,
            "native-provider"
        );

        let runner = ExtensionRunner::new(
            vec![extension],
            Arc::clone(&runtime),
            "/fixture/project".to_string(),
        );
        assert_eq!(
            runner.execute_command("echo", "hello").expect("command"),
            Some(json!({"args": "hello"}))
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
                .expect("tool"),
            json!({"id": "call-1", "params": {"value": 7}})
        );
    }

    #[test]
    fn factory_errors_and_panics_are_reported_without_a_loaded_extension() {
        let error = load_extension_from_factory(
            |_api| Err("factory failed".to_string()),
            "/fixture/project",
            create_extension_runtime(),
            "<inline:error>",
        )
        .expect_err("factory error");
        assert!(error.error.contains("Failed to load Rust extension"));
        assert!(error.error.contains("factory failed"));

        let panic = load_extension_from_factory(
            |_api| -> Result<(), String> { panic!("factory panic") },
            "/fixture/project",
            create_extension_runtime(),
            "<inline:panic>",
        )
        .expect_err("factory panic");
        assert!(panic.error.contains("factory panicked"));
        assert!(panic.error.contains("factory panic"));
    }

    #[test]
    fn host_action_loading_binds_a_rust_runtime_without_a_bridge() {
        let result = load_extensions_with_host_actions(
            &[],
            "/fixture/project",
            None,
            Some("ignored"),
            Arc::new(TestHost),
        );
        let runtime = result.runtime.lock().expect("runtime lock");
        assert!(runtime.is_initialized());
        assert!(runtime.has_host_actions());
    }

    #[test]
    fn resolve_relative_path_retains_normalization_contract() {
        let base = sandbox("paths");
        let nested = resolve_relative_path("./nested/../entry.ts", &base.to_string_lossy());
        assert_eq!(nested, base.join("entry.ts"));
        let unicode = resolve_relative_path("module\u{00a0}name.ts", &base.to_string_lossy());
        assert_eq!(unicode, base.join("module name.ts"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn configured_directory_does_not_auto_select_unsupported_entries() {
        let root = sandbox("configured-dir");
        let extension_dir = root.join("extensions");
        fs::create_dir_all(&extension_dir).expect("create extension dir");
        let result = discover_and_load_extensions(
            &[extension_dir.to_string_lossy().into_owned()],
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
    fn configured_directory_ignores_unsupported_manifest_entries() {
        let root = sandbox("configured-manifest");
        let extension_dir = root.join("extensions");
        fs::create_dir_all(&extension_dir).expect("create extension dir");
        fs::write(
            extension_dir.join("package.json"),
            r#"{ "pi": { "extensions": ["entry.ts"] } }"#,
        )
        .expect("write package manifest");

        let result = discover_and_load_extensions(
            &[extension_dir.to_string_lossy().into_owned()],
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
    fn context_type_remains_available_to_native_factories() {
        let context = ExtensionContext {
            mode: "print".to_string(),
            cwd: "/fixture/project".to_string(),
            has_ui: false,
        };
        assert_eq!(context.mode, "print");
    }
}
