//! SettingsManager oracle tests — ported from the upstream suite
//! `packages/coding-agent/test/settings-manager.test.ts` (pinned 5cd93f6),
//! plus seam tests for the Rust facade. Split into slices:
//!   B) in-memory/facade machinery; C) file-backed; D) trust/reload/errors; E) packages.

use std::fs;
use std::path::PathBuf;

use indexmap::IndexMap;

/// Serializes tests that mutate the process-global VISUAL/EDITOR env vars.
fn editor_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap()
}
use pi_coding_agent::core::settings::{
    deep_merge, migrate_settings, parse_http_idle_timeout_ms, strip_bom, InMemorySettingsStorage,
    SettingsManager, SettingsScope, SettingsStorage,
};
use serde_json::{json, Value};

fn map(v: Value) -> IndexMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

/// Two-scope in-memory storage pre-seeded with given contents.
fn seeded_storage(global: Value, project: Value) -> InMemorySettingsStorage {
    let storage = InMemorySettingsStorage::new();
    if !global.is_null() {
        storage.with_lock(SettingsScope::Global, &mut |_| Some(global.to_string()));
    }
    if !project.is_null() {
        storage.with_lock(SettingsScope::Project, &mut |_| Some(project.to_string()));
    }
    storage
}

fn write_seeded(storage: &dyn SettingsStorage, scope: SettingsScope, content: Value) {
    let content = content.to_string();
    storage.with_lock(scope, &mut |_| Some(content.clone()));
}

fn read_seeded(storage: &dyn SettingsStorage, scope: SettingsScope) -> Value {
    let mut out = Value::Null;
    storage.with_lock(scope, &mut |current| {
        out = serde_json::from_str(current.unwrap_or("null")).unwrap();
        None
    });
    out
}

// ---------------------------------------------------------------------------
// Slice B — in-memory / facade machinery
// ---------------------------------------------------------------------------

#[test]
fn b_default_tools_defaults_and_empty_list() {
    let m = SettingsManager::in_memory(map(json!({ "defaultTools": [] })));
    assert_eq!(m.get_default_tools(), Some(vec![]));
    let m = SettingsManager::in_memory(map(json!({})));
    assert_eq!(m.get_default_tools(), None);
}

#[test]
fn b_in_memory_theme_round_trip() {
    let m = SettingsManager::in_memory(map(json!({ "theme": "dark" })));
    assert_eq!(m.get_theme(), Some("dark".to_string()));
    assert_eq!(m.get_theme_setting(), Some("dark"));
}

#[test]
fn b_slash_theme_is_theme_setting_only() {
    let m = SettingsManager::in_memory(map(json!({ "theme": "light/dark" })));
    assert_eq!(m.get_theme(), None);
    assert_eq!(m.get_theme_setting(), Some("light/dark"));
}

#[test]
fn b_project_overrides_global_for_merge() {
    let storage = seeded_storage(
        json!({ "defaultTools": ["read"] }),
        json!({ "defaultTools": ["grep"] }),
    );
    let m = SettingsManager::from_storage(Box::new(storage));
    assert_eq!(m.get_default_tools(), Some(vec!["grep".to_string()]));
    // Scope views are separate
    assert_eq!(
        m.get_global_settings().get("defaultTools").unwrap(),
        &json!(["read"])
    );
    assert_eq!(
        m.get_project_settings().get("defaultTools").unwrap(),
        &json!(["grep"])
    );
}

#[test]
fn b_merge_keeps_global_nested_untouched_when_project_only_has_one_key() {
    let storage = seeded_storage(
        json!({ "compaction": { "enabled": true, "reserveTokens": 8192 } }),
        json!({ "compaction": { "reserveTokens": 100 } }),
    );
    let m = SettingsManager::from_storage(Box::new(storage));
    assert_eq!(m.get_compaction_settings(), (true, 100, 20000));
}

#[test]
fn b_migration_applies_on_load() {
    let m = SettingsManager::in_memory(map(json!({ "queueMode": "one-at-a-time" })));
    assert_eq!(m.get_steering_mode(), "one-at-a-time");
    let m = SettingsManager::in_memory(map(json!({ "websockets": true })));
    assert_eq!(m.get_transport(), "websocket");
    let m = SettingsManager::in_memory(map(json!({ "websockets": false })));
    assert_eq!(m.get_transport(), "sse");
}

#[test]
fn b_getter_defaults() {
    let m = SettingsManager::in_memory(map(json!({})));
    assert_eq!(m.get_default_provider(), None);
    assert_eq!(m.get_default_model(), None);
    assert_eq!(m.get_default_thinking_level(), None);
    assert_eq!(m.get_transport(), "auto");
    assert!(m.get_compaction_enabled());
    assert!(m.get_retry_enabled());
    assert_eq!(m.get_http_idle_timeout_ms().unwrap(), 300_000);
    assert_eq!(m.get_output_pad(), 1);
    assert_eq!(m.get_tui_mode(), "regular");
    assert_eq!(m.get_fullscreen_exit_output(), "transcript");
    assert_eq!(m.get_fullscreen_scrollbar(), "auto");
    assert_eq!(m.get_mermaid_rendering_mode(), "streaming");
    assert_eq!(m.get_default_project_trust(), "ask");
    assert_eq!(m.get_steering_mode(), "one-at-a-time");
    assert_eq!(m.get_follow_up_mode(), "one-at-a-time");
}

#[test]
fn b_http_idle_timeout_invalid_errors() {
    let m = SettingsManager::in_memory(map(json!({ "httpIdleTimeoutMs": -1 })));
    let err = m.get_http_idle_timeout_ms().unwrap_err();
    assert!(
        err.contains("Invalid httpIdleTimeoutMs setting"),
        "got: {err}"
    );
}

#[tokio::test]
async fn b_set_and_persist_in_memory_preserves_unknown_keys() {
    let storage = InMemorySettingsStorage::new();
    let mut m = SettingsManager::from_storage(Box::new(storage));
    m.set_theme("dark".to_string());
    // Simulate external write of an unknown key while manager holds the file
    // via storage: only possible through the same storage (in-memory seam).
    write_seeded(
        m.storage().as_ref(),
        SettingsScope::Global,
        json!({ "enabledModels": ["x"] }),
    );
    m.set_default_thinking_level("high");
    m.flush().await;
    let saved = read_seeded(m.storage().as_ref(), SettingsScope::Global);
    assert_eq!(saved.get("theme"), Some(&json!("dark")));
    assert_eq!(saved.get("defaultThinkingLevel"), Some(&json!("high")));
    assert_eq!(saved.get("enabledModels"), Some(&json!(["x"])));
}

// ---------------------------------------------------------------------------
// Pure helpers exposed through the facade (regression seams for slice A)
// ---------------------------------------------------------------------------

#[test]
fn b_pure_helpers_are_public() {
    let mut a = map(json!({ "x": { "y": 1 } }));
    let b = map(json!({ "x": { "z": 2 } }));
    deep_merge(&mut a, &b);
    assert_eq!(a, map(json!({ "x": { "y": 1, "z": 2 } })));
    let mut s = map(json!({ "websockets": true }));
    migrate_settings(&mut s);
    assert_eq!(s, map(json!({ "transport": "websocket" })));
    assert_eq!(parse_http_idle_timeout_ms(&json!("disabled")), Some(0));
    assert_eq!(strip_bom("\u{FEFF}{}"), "{}");
}

// ---------------------------------------------------------------------------
// Slice C — file-backed persistence (upstream oracle)
// ---------------------------------------------------------------------------

struct TestDirs {
    root: PathBuf,
    agent_dir: PathBuf,
    project_dir: PathBuf,
}

impl TestDirs {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("pi-settings-sm-{}", uuid::Uuid::new_v4()));
        let agent_dir = root.join("agent");
        let project_dir = root.join("project");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(project_dir.join(".pi")).unwrap();
        Self {
            root,
            agent_dir,
            project_dir,
        }
    }

    fn write_global(&self, v: Value) {
        fs::write(self.agent_dir.join("settings.json"), v.to_string()).unwrap();
    }

    fn write_project(&self, v: Value) {
        fs::write(
            self.project_dir.join(".pi").join("settings.json"),
            v.to_string(),
        )
        .unwrap();
    }

    fn read_global(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.agent_dir.join("settings.json")).unwrap())
            .unwrap()
    }

    fn read_project(&self) -> Value {
        serde_json::from_str(
            &fs::read_to_string(self.project_dir.join(".pi").join("settings.json")).unwrap(),
        )
        .unwrap()
    }

    fn global_path(&self) -> String {
        format!("{}/settings.json", self.agent_dir.display())
    }

    fn project_path(&self) -> String {
        format!("{}/.pi/settings.json", self.project_dir.display())
    }

    fn manager(&self) -> SettingsManager {
        SettingsManager::create(
            &self.project_dir.display().to_string(),
            &self.agent_dir.display().to_string(),
            Default::default(),
        )
    }
}

impl Drop for TestDirs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn c_preserves_enabled_models_when_changing_thinking_level() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark", "defaultModel": "claude-sonnet" }));

    let mut manager = dirs.manager();
    // External edit adds enabledModels.
    let mut current = dirs.read_global();
    current.as_object_mut().unwrap().insert(
        "enabledModels".into(),
        json!(["claude-opus-4-5", "gpt-5.2-codex"]),
    );
    dirs.write_global(current);

    manager.set_default_thinking_level("high");
    manager.flush().await;

    let saved = dirs.read_global();
    assert_eq!(
        saved.get("enabledModels"),
        Some(&json!(["claude-opus-4-5", "gpt-5.2-codex"]))
    );
    assert_eq!(saved.get("defaultThinkingLevel"), Some(&json!("high")));
    assert_eq!(saved.get("theme"), Some(&json!("dark")));
    assert_eq!(saved.get("defaultModel"), Some(&json!("claude-sonnet")));
}

#[tokio::test]
async fn c_preserves_custom_settings_when_changing_theme() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "defaultModel": "claude-sonnet" }));

    let mut manager = dirs.manager();
    let mut current = dirs.read_global();
    let obj = current.as_object_mut().unwrap();
    obj.insert("shellPath".into(), json!("/bin/zsh"));
    obj.insert("extensions".into(), json!(["/path/to/extension.ts"]));
    dirs.write_global(current);

    manager.set_theme("light".to_string());
    manager.flush().await;

    let saved = dirs.read_global();
    assert_eq!(saved.get("shellPath"), Some(&json!("/bin/zsh")));
    assert_eq!(
        saved.get("extensions"),
        Some(&json!(["/path/to/extension.ts"]))
    );
    assert_eq!(saved.get("theme"), Some(&json!("light")));
}

#[tokio::test]
async fn c_in_memory_changes_override_file_changes_for_same_key() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));

    let mut manager = dirs.manager();
    // External edit sets thinking level low.
    let mut current = dirs.read_global();
    current
        .as_object_mut()
        .unwrap()
        .insert("defaultThinkingLevel".into(), json!("low"));
    dirs.write_global(current);

    // In-memory override to high wins on flush.
    manager.set_default_thinking_level("high");
    manager.flush().await;

    let saved = dirs.read_global();
    assert_eq!(saved.get("defaultThinkingLevel"), Some(&json!("high")));
}

#[tokio::test]
async fn c_slash_theme_persists_raw() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "light/dark" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_theme(), None);
    assert_eq!(manager.get_theme_setting(), Some("light/dark"));

    let mut manager = manager;
    manager.set_theme("solarized-light/tokyo-night".to_string());
    manager.flush().await;
    let saved = dirs.read_global();
    assert_eq!(
        saved.get("theme"),
        Some(&json!("solarized-light/tokyo-night"))
    );
}

#[tokio::test]
async fn c_output_pad_defaults_and_persists_binary() {
    let dirs = TestDirs::new();
    let mut manager = dirs.manager();
    assert_eq!(manager.get_output_pad(), 1);

    manager.set_output_pad(0);
    manager.flush().await;
    assert_eq!(manager.get_output_pad(), 0);
    let saved = dirs.read_global();
    assert_eq!(saved.get("outputPad"), Some(&json!(0)));
}

#[test]
fn c_output_pad_unsupported_value_defaults() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "outputPad": 2 }));
    let manager = dirs.manager();
    assert_eq!(manager.get_output_pad(), 1);
}

#[tokio::test]
async fn c_mermaid_defaults_and_persists() {
    let dirs = TestDirs::new();
    let mut manager = dirs.manager();
    assert_eq!(manager.get_mermaid_rendering_mode(), "streaming");

    manager.set_mermaid_rendering_mode("final");
    manager.flush().await;
    assert_eq!(manager.get_mermaid_rendering_mode(), "final");
    let saved = dirs.read_global();
    assert_eq!(
        saved.get("markdown").and_then(|v| v.get("mermaid")),
        Some(&json!("final"))
    );
}

#[test]
fn c_mermaid_unsupported_value_falls_back() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "markdown": { "mermaid": "sometimes" } }));
    let manager = dirs.manager();
    assert_eq!(manager.get_mermaid_rendering_mode(), "streaming");
}

#[test]
fn c_shell_command_prefix_loads() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "shellCommandPrefix": "shopt -s expand_aliases" }));
    let manager = dirs.manager();
    assert_eq!(
        manager.get_shell_command_prefix(),
        Some("shopt -s expand_aliases")
    );
}

#[test]
fn c_shell_command_prefix_unset_is_none() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_shell_command_prefix(), None);
}

#[tokio::test]
async fn c_shell_command_prefix_preserved_when_saving_other_settings() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "shellCommandPrefix": "shopt -s expand_aliases" }));
    let mut manager = dirs.manager();
    manager.set_theme("light".to_string());
    manager.flush().await;
    let saved = dirs.read_global();
    assert_eq!(
        saved.get("shellCommandPrefix"),
        Some(&json!("shopt -s expand_aliases"))
    );
    assert_eq!(saved.get("theme"), Some(&json!("light")));
}

#[tokio::test]
async fn c_fullscreen_settings_validate_and_persist() {
    let dirs = TestDirs::new();
    let mut manager = dirs.manager();
    assert_eq!(manager.get_fullscreen_exit_output(), "transcript");
    assert_eq!(manager.get_fullscreen_scrollbar(), "auto");

    manager.set_fullscreen_exit_output("resume-hint");
    manager.set_fullscreen_scrollbar("hidden");
    manager.flush().await;
    let saved = dirs.read_global();
    assert_eq!(
        saved.get("fullscreenExitOutput"),
        Some(&json!("resume-hint"))
    );
    assert_eq!(saved.get("fullscreenScrollbar"), Some(&json!("hidden")));

    // Unsupported values fall back on next load.
    dirs.write_global(
        json!({ "fullscreenExitOutput": "nothing", "fullscreenScrollbar": "sometimes" }),
    );
    let reloaded = dirs.manager();
    assert_eq!(reloaded.get_fullscreen_exit_output(), "transcript");
    assert_eq!(reloaded.get_fullscreen_scrollbar(), "auto");
}

#[test]
fn c_default_tools_global_then_project_replace() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "defaultTools": ["read", "bash"] }));
    let manager = dirs.manager();
    assert_eq!(
        manager.get_default_tools(),
        Some(vec!["read".to_string(), "bash".to_string()])
    );

    dirs.write_project(json!({ "defaultTools": ["grep"] }));
    let manager = dirs.manager();
    assert_eq!(manager.get_default_tools(), Some(vec!["grep".to_string()]));
}

// ---------------------------------------------------------------------------
// Slice D — reload, errors, project trust, directory creation, remaining groups
// ---------------------------------------------------------------------------

#[tokio::test]
async fn d_reload_global_from_disk() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark", "extensions": ["/before.ts"] }));
    let mut manager = dirs.manager();

    dirs.write_global(
        json!({ "theme": "light", "extensions": ["/after.ts"], "defaultModel": "claude-sonnet" }),
    );
    manager.reload().await;

    assert_eq!(manager.get_theme(), Some("light".to_string()));
    assert_eq!(manager.get_extension_paths(), vec!["/after.ts".to_string()]);
    assert_eq!(manager.get_default_model(), Some("claude-sonnet"));
}

#[tokio::test]
async fn d_reload_keeps_previous_settings_and_reports_path_when_invalid() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));
    let mut manager = dirs.manager();

    fs::write(dirs.agent_dir.join("settings.json"), "{ invalid json").unwrap();
    manager.reload().await;

    assert_eq!(manager.get_theme(), Some("dark".to_string()));
    let errors = manager.drain_errors();
    assert_eq!(errors[0].scope, SettingsScope::Global);
    assert_eq!(errors[0].path.as_deref(), Some(dirs.global_path().as_str()));
}

#[test]
fn d_drain_errors_collects_both_scopes() {
    let dirs = TestDirs::new();
    fs::write(
        dirs.agent_dir.join("settings.json"),
        "{ invalid global json",
    )
    .unwrap();
    fs::write(
        dirs.project_dir.join(".pi").join("settings.json"),
        "{ invalid project json",
    )
    .unwrap();

    let mut manager = dirs.manager();
    let errors = manager.drain_errors();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].scope, SettingsScope::Global);
    assert_eq!(errors[0].path.as_deref(), Some(dirs.global_path().as_str()));
    assert_eq!(errors[1].scope, SettingsScope::Project);
    assert_eq!(
        errors[1].path.as_deref(),
        Some(dirs.project_path().as_str())
    );
    assert_eq!(manager.drain_errors().len(), 0);
}

#[test]
fn d_project_untrusted_skips_project_settings() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "global" }));
    dirs.write_project(json!({ "theme": "project" }));

    let manager = SettingsManager::create(
        &dirs.project_dir.display().to_string(),
        &dirs.agent_dir.display().to_string(),
        pi_coding_agent::core::settings::SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );

    assert!(!manager.is_project_trusted());
    assert_eq!(manager.get_theme(), Some("global".to_string()));
    assert!(manager.get_project_settings().is_empty());
}

#[test]
fn d_project_trust_change_reloads_project_settings() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "global" }));
    dirs.write_project(json!({ "theme": "project" }));

    let mut manager = SettingsManager::create(
        &dirs.project_dir.display().to_string(),
        &dirs.agent_dir.display().to_string(),
        pi_coding_agent::core::settings::SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    manager.set_project_trusted(true);

    assert!(manager.is_project_trusted());
    assert_eq!(manager.get_theme(), Some("project".to_string()));
}

#[tokio::test]
async fn d_project_untrusted_fails_writes() {
    let dirs = TestDirs::new();
    dirs.write_project(json!({ "packages": ["npm:existing"] }));

    let mut manager = SettingsManager::create(
        &dirs.project_dir.display().to_string(),
        &dirs.agent_dir.display().to_string(),
        pi_coding_agent::core::settings::SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.set_project_packages(vec![pi_coding_agent::core::settings::PackageSource::Str(
            "npm:new".to_string(),
        )]);
    }));
    assert!(
        result.is_err(),
        "set_project_packages should panic when untrusted"
    );

    manager.flush().await;
    assert!(manager.get_project_settings().is_empty());
    assert_eq!(dirs.read_project(), json!({ "packages": ["npm:existing"] }));
}

#[test]
fn d_default_project_trust_reads_global_only() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "defaultProjectTrust": "always" }));
    dirs.write_project(json!({ "defaultProjectTrust": "never" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_default_project_trust(), "always");
}

#[test]
fn d_default_project_trust_invalid_is_ask() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "defaultProjectTrust": "sometimes" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_default_project_trust(), "ask");
}

#[test]
fn d_reading_project_settings_does_not_create_pi_dir() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));
    fs::remove_dir_all(dirs.project_dir.join(".pi")).unwrap();

    let manager = dirs.manager();
    assert!(!dirs.project_dir.join(".pi").exists());
    assert_eq!(manager.get_theme(), Some("dark".to_string()));
}

#[tokio::test]
async fn d_writing_project_settings_creates_pi_dir() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));
    fs::remove_dir_all(dirs.project_dir.join(".pi")).unwrap();

    let mut manager = dirs.manager();
    assert!(!dirs.project_dir.join(".pi").exists());

    manager.set_project_packages(vec![pi_coding_agent::core::settings::PackageSource::Obj(
        pi_coding_agent::core::settings::PackageSourceObj {
            source: "npm:test-pkg".to_string(),
            ..Default::default()
        },
    )]);
    manager.flush().await;

    assert!(dirs.project_dir.join(".pi").exists());
    assert!(dirs.project_dir.join(".pi").join("settings.json").exists());
}

#[test]
fn d_http_idle_timeout_defaults_to_5_minutes() {
    let dirs = TestDirs::new();
    let manager = dirs.manager();
    assert_eq!(manager.get_http_idle_timeout_ms().unwrap(), 300_000);
}

#[test]
fn d_http_idle_timeout_merges_global_and_project() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "httpIdleTimeoutMs": 300000 }));
    dirs.write_project(json!({ "httpIdleTimeoutMs": 0 }));
    let manager = dirs.manager();
    assert_eq!(manager.get_http_idle_timeout_ms().unwrap(), 0);
}

#[test]
fn d_http_idle_timeout_rejects_invalid() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "httpIdleTimeoutMs": -1 }));
    let manager = dirs.manager();
    assert!(manager.get_http_idle_timeout_ms().is_err());
}

fn set_editor_env(visual: Option<&str>, editor: Option<&str>) {
    for (var, val) in [("VISUAL", visual), ("EDITOR", editor)] {
        match val {
            Some(v) => unsafe { std::env::set_var(var, v) },
            None => unsafe { std::env::remove_var(var) },
        }
    }
}

#[test]
fn d_external_editor_resolves_by_precedence() {
    let _guard = editor_env_lock();
    set_editor_env(Some("vim"), Some("nano"));
    let m = SettingsManager::in_memory(map(json!({ "externalEditor": "code --wait" })));
    assert_eq!(m.get_external_editor_command(), "code --wait");

    let m = SettingsManager::in_memory(map(json!({})));
    assert_eq!(m.get_external_editor_command(), "vim");

    set_editor_env(None, Some("emacs"));
    let m = SettingsManager::in_memory(map(json!({})));
    assert_eq!(m.get_external_editor_command(), "emacs");
    set_editor_env(None, None);
}

#[test]
fn d_external_editor_falls_back_to_platform_defaults() {
    let _guard = editor_env_lock();
    set_editor_env(None, None);
    let m = SettingsManager::in_memory(map(json!({})));
    let cmd = m.get_external_editor_command();
    // Platform-dependent by design; assert the fallback is non-empty and one of
    // the known values for the current platform.
    if cfg!(windows) {
        assert_eq!(cmd, "notepad");
    } else {
        assert_eq!(cmd, "nano");
    }
}

#[tokio::test]
async fn d_tui_mode_defaults_regular_and_persists_fullscreen() {
    let dirs = TestDirs::new();
    let mut manager = dirs.manager();
    assert_eq!(manager.get_tui_mode(), "regular");

    manager.set_tui_mode("fullscreen");
    manager.flush().await;
    assert_eq!(manager.get_tui_mode(), "fullscreen");
    let saved = dirs.read_global();
    assert_eq!(saved.get("tuiMode"), Some(&json!("fullscreen")));
}

#[test]
fn d_tui_mode_unsupported_value_falls_back() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "tuiMode": "other" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_tui_mode(), "regular");
}

#[test]
fn d_ui_mode_setting_is_not_recognized() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "uiMode": "fullscreen" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_tui_mode(), "regular");
}

#[test]
fn d_session_dir_variants() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_session_dir(), None);

    dirs.write_global(json!({ "sessionDir": "/tmp/sessions" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_session_dir(), Some("/tmp/sessions".to_string()));

    dirs.write_global(json!({ "sessionDir": "/global/sessions" }));
    dirs.write_project(json!({ "sessionDir": "./sessions" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_session_dir(), Some("./sessions".to_string()));

    dirs.write_project(json!({}));
    dirs.write_global(json!({ "sessionDir": "~/sessions" }));
    let manager = dirs.manager();
    let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
    assert_eq!(manager.get_session_dir(), Some(format!("{home}/sessions")));
}

#[test]
fn d_shell_path_variants() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "theme": "dark" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_shell_path(), None);

    dirs.write_global(json!({ "shellPath": "/bin/zsh" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_shell_path(), Some("/bin/zsh".to_string()));

    dirs.write_global(json!({ "shellPath": "~/.local/bin/agent-shell-sandbox" }));
    let manager = dirs.manager();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        manager.get_shell_path(),
        Some(format!("{home}/.local/bin/agent-shell-sandbox"))
    );

    dirs.write_global(json!({ "shellPath": "~" }));
    let manager = dirs.manager();
    assert_eq!(manager.get_shell_path(), Some(home.clone()));
}

#[test]
fn d_packages_migration_local_extensions_kept() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "extensions": ["/local/ext.ts", "./relative/ext.ts"] }));
    let manager = dirs.manager();
    assert_eq!(manager.get_packages(), vec![]);
    assert_eq!(
        manager.get_extension_paths(),
        vec!["/local/ext.ts".to_string(), "./relative/ext.ts".to_string()]
    );
}

#[test]
fn d_packages_migration_filtering_objects() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({
        "packages": [
            "npm:simple-pkg",
            { "source": "npm:shitty-extensions", "extensions": ["extensions/oracle.ts"], "skills": [] }
        ]
    }));
    let manager = dirs.manager();
    let packages = manager.get_packages();
    assert_eq!(packages.len(), 2);
    assert_eq!(
        packages[0],
        pi_coding_agent::core::settings::PackageSource::Str("npm:simple-pkg".to_string())
    );
    assert_eq!(
        packages[1],
        pi_coding_agent::core::settings::PackageSource::Obj(
            pi_coding_agent::core::settings::PackageSourceObj {
                source: "npm:shitty-extensions".to_string(),
                autoload: None,
                extensions: Some(vec!["extensions/oracle.ts".to_string()]),
                skills: Some(vec![]),
                prompts: None,
                themes: None,
            }
        )
    );
}

// ---------------------------------------------------------------------------
// Slice E — review findings regressions (provider retry depth, key removal)
// ---------------------------------------------------------------------------

#[test]
fn e_provider_retry_settings_read_from_retry_provider() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({
        "retry": {
            "enabled": false,
            "maxRetries": 5,
            "baseDelayMs": 100,
            "provider": { "timeoutMs": 1234, "maxRetries": 7, "maxRetryDelayMs": 9999 }
        }
    }));
    let manager = dirs.manager();
    assert_eq!(manager.get_retry_settings(), (false, 5, 100));
    assert_eq!(
        manager.get_provider_retry_settings(),
        (Some(1234), Some(7), 9999)
    );
}

#[test]
fn e_provider_retry_settings_defaults() {
    let dirs = TestDirs::new();
    let manager = dirs.manager();
    assert_eq!(manager.get_provider_retry_settings(), (None, None, 60000));
    assert_eq!(manager.get_retry_settings(), (true, 3, 2000));
}

#[tokio::test]
async fn e_setting_none_removes_key_from_file() {
    let dirs = TestDirs::new();
    dirs.write_global(json!({ "shellPath": "/bin/zsh", "shellCommandPrefix": "x", "npmCommand": ["npm"], "enabledModels": ["a"] }));
    let mut manager = dirs.manager();
    manager.set_shell_path(None);
    manager.set_shell_command_prefix(None);
    manager.set_npm_command(None);
    manager.set_enabled_models(None);
    manager.flush().await;

    let saved = dirs.read_global();
    assert!(saved.get("shellPath").is_none());
    assert!(saved.get("shellCommandPrefix").is_none());
    assert!(saved.get("npmCommand").is_none());
    assert!(saved.get("enabledModels").is_none());

    // And the setters' file-level removal survives a reload.
    let manager = dirs.manager();
    assert_eq!(manager.get_shell_path(), None);
    assert_eq!(manager.get_npm_command(), None);
}
