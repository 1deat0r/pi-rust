//! Settings manager — port of `packages/coding-agent/src/core/settings-manager.ts`
//! (1:1 behavior; the upstream test suite is the oracle).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{self, CONFIG_DIR_NAME};

/// A settings object (global or project scope) as an ordered map.
pub type SettingsMap = IndexMap<String, Value>;

// ---------------------------------------------------------------------------
// Pure helpers (ported from settings-manager.ts + http-dispatcher.ts + text.ts)
// ---------------------------------------------------------------------------

/// Deep merge JSON maps: objects merge recursively, everything else is
/// overridden. Mirrors `deepMergeObjects` (undefined is skipped; JSON never
/// carries undefined).
pub fn deep_merge(base: &mut SettingsMap, overrides: &SettingsMap) {
    for (key, override_value) in overrides {
        match (base.get(key), override_value) {
            (Some(Value::Object(base_obj)), Value::Object(override_obj)) => {
                let mut base_map: SettingsMap =
                    base_obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let override_map: SettingsMap = override_obj
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                deep_merge(&mut base_map, &override_map);
                base.insert(key.clone(), Value::Object(base_map.into_iter().collect()));
            }
            _ => {
                base.insert(key.clone(), override_value.clone());
            }
        }
    }
}

/// Migrate legacy settings keys to the current format (mutates in place).
pub fn migrate_settings(settings: &mut SettingsMap) {
    // Migrate queueMode -> steeringMode
    if settings.contains_key("queueMode") && !settings.contains_key("steeringMode") {
        if let Some(v) = settings.get("queueMode").cloned() {
            settings.insert("steeringMode".to_string(), v);
        }
        settings.shift_remove("queueMode");
    }

    // Migrate legacy websockets boolean -> transport enum
    if !settings.contains_key("transport") {
        if let Some(Value::Bool(ws)) = settings.get("websockets") {
            settings.insert(
                "transport".to_string(),
                Value::String(if *ws { "websocket" } else { "sse" }.to_string()),
            );
            settings.shift_remove("websockets");
        }
    }

    // Migrate old skills object format to new array format
    if let Some(Value::Object(skills_obj)) = settings.get("skills") {
        let skills_map: SettingsMap =
            skills_obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if let Some(Value::Bool(enable)) = skills_map.get("enableSkillCommands") {
            if !settings.contains_key("enableSkillCommands") {
                settings.insert("enableSkillCommands".to_string(), Value::Bool(*enable));
            }
        }
        let dirs = skills_map.get("customDirectories").and_then(|v| v.as_array());
        match dirs {
            Some(dirs) if !dirs.is_empty() => {
                settings.insert("skills".to_string(), Value::Array(dirs.clone()));
            }
            _ => {
                settings.shift_remove("skills");
            }
        }
    }

    // Migrate retry.maxDelayMs -> retry.provider.maxRetryDelayMs
    if let Some(Value::Object(retry_obj)) = settings.get("retry") {
        let mut retry_map: SettingsMap =
            retry_obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let provider_settings = retry_map
            .get("provider")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<SettingsMap>());
        if let Some(Value::Number(max_delay)) = retry_map.get("maxDelayMs") {
            let provider_max = provider_settings
                .as_ref()
                .and_then(|p| p.get("maxRetryDelayMs"));
            let provider_max_missing = matches!(provider_max, None | Some(Value::Null));
            if provider_max_missing {
                let mut provider = provider_settings.clone().unwrap_or_default();
                provider.insert("maxRetryDelayMs".to_string(), Value::Number(max_delay.clone()));
                retry_map.insert("provider".to_string(), Value::Object(provider.into_iter().collect()));
            }
        }
        retry_map.shift_remove("maxDelayMs");
        settings.insert("retry".to_string(), Value::Object(retry_map.into_iter().collect()));
    }
}

/// HTTP idle timeout parser (mirrors `parseHttpIdleTimeoutMs`).
/// `"disabled"`/`DISABLED` -> 0; empty string -> None; non-finite or negative
/// -> None; otherwise floor.
pub fn parse_http_idle_timeout_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            let n = trimmed.parse::<f64>().ok()?;
            parse_http_idle_timeout_ms(&Value::Number(serde_json::Number::from_f64(n)?))
        }
        Value::Number(n) => {
            let f = n.as_f64()?;
            if !f.is_finite() || f < 0.0 {
                return None;
            }
            Some(f.floor() as u64)
        }
        _ => None,
    }
}

/// Strip a UTF-8 BOM from the start of a string (mirrors `stripBom`).
pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{FEFF}').unwrap_or(content)
}


// ---------------------------------------------------------------------------
// Settings storage layer (ported from settings-manager.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsScope {
    Global,
    Project,
}

/// A settings load/write error recorded for later diagnosis.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub path: Option<String>,
    pub error: String,
}

impl SettingsError {
    pub fn new(scope: SettingsScope, error: impl Into<String>, path: Option<String>) -> Self {
        Self { scope, path, error: error.into() }
    }
}

/// Storage backends expose a scoped, lock-protected read-modify-write.
pub trait SettingsStorage: Send + Sync {
    /// Runs `f` with the current content (None when the file does not exist).
    /// Write back when `f` returns `Some(next)`.
    fn with_lock(
        &self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<&str>) -> Option<String>,
    );
}

/// In-memory backend (used by `SettingsManager::in_memory` and tests).
pub struct InMemorySettingsStorage {
    global: Mutex<Option<String>>,
    project: Mutex<Option<String>>,
}

impl InMemorySettingsStorage {
    pub fn new() -> Self {
        Self { global: Mutex::new(None), project: Mutex::new(None) }
    }
}

impl Default for InMemorySettingsStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(
        &self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<&str>) -> Option<String>,
    ) {
        let slot = match scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Project => &self.project,
        };
        let mut guard = slot.lock().unwrap();
        let next = f(guard.as_deref());
        if let Some(next) = next {
            *guard = Some(next);
        }
    }
}

/// Holds an exclusive `.lock` file; removes it on drop (like upstream
/// proper-lockfile release).
struct LockGuard {
    _file: fs::File,
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// File backend with a sibling `.lock` file retried like upstream lockfile.
pub struct FileSettingsStorage {
    global_settings_path: PathBuf,
    project_settings_path: PathBuf,
}

impl FileSettingsStorage {
    pub fn new(cwd: &str, agent_dir: &str) -> Self {
        Self {
            global_settings_path: PathBuf::from(agent_dir).join("settings.json"),
            project_settings_path: PathBuf::from(cwd).join(CONFIG_DIR_NAME).join("settings.json"),
        }
    }

    pub fn path_for(&self, scope: SettingsScope) -> PathBuf {
        match scope {
            SettingsScope::Global => self.global_settings_path.clone(),
            SettingsScope::Project => self.project_settings_path.clone(),
        }
    }

    fn acquire_lock_with_retry(path: &std::path::Path) -> LockGuard {
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        const MAX_ATTEMPTS: u32 = 10;
        const DELAY_MS: u64 = 20;
        let mut last_error: Option<std::io::Error> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
                Ok(file) => return LockGuard { _file: file, path: lock_path },
                Err(e) if e.kind() == ErrorKind::AlreadyExists && attempt < MAX_ATTEMPTS => {
                    last_error = Some(e);
                    thread::sleep(Duration::from_millis(DELAY_MS));
                }
                Err(e) => {
                    last_error = Some(e);
                    break;
                }
            }
        }
        panic!(
            "Failed to acquire settings lock for {}: {}",
            lock_path.display(),
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(
        &self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<&str>) -> Option<String>,
    ) {
        let path = self.path_for(scope);
        let file_exists = path.exists();
        let mut _lock_guard = if file_exists {
            Some(Self::acquire_lock_with_retry(&path))
        } else {
            None
        };
        let current = if file_exists {
            Some(fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("Failed to read settings file {}: {e}", path.display())
            }))
        } else {
            None
        };
        let next = f(current.as_deref());
        if let Some(next) = next {
            if let Some(dir) = path.parent() {
                if !dir.exists() {
                    fs::create_dir_all(dir)
                        .unwrap_or_else(|e| panic!("Failed to create settings dir {dir:?}: {e}"));
                }
            }
            if _lock_guard.is_none() {
                _lock_guard = Some(Self::acquire_lock_with_retry(&path));
            }
            fs::write(&path, next)
                .unwrap_or_else(|e| panic!("Failed to write settings file {}: {e}", path.display()));
        }
    }
}

// ---------------------------------------------------------------------------
// Package source (settings-manager.ts `PackageSource`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    Str(String),
    Obj(PackageSourceObj),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PackageSourceObj {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Settings manager (ported from SettingsManager)
// ---------------------------------------------------------------------------

type WriteTask = Box<dyn FnOnce() + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsManagerCreateOptions {
    pub project_trusted: bool,
}

impl Default for SettingsManagerCreateOptions {
    fn default() -> Self {
        Self { project_trusted: true }
    }
}

pub struct SettingsManager {
    storage: Arc<dyn SettingsStorage>,
    global_settings: SettingsMap,
    project_settings: SettingsMap,
    settings: SettingsMap,
    project_trusted: bool,
    modified_fields: HashSet<String>,
    modified_nested: HashMap<String, HashSet<String>>,
    modified_project_fields: HashSet<String>,
    modified_project_nested: HashMap<String, HashSet<String>>,
    global_load_error: Option<SettingsError>,
    project_load_error: Option<SettingsError>,
    queue: Mutex<VecDeque<(SettingsScope, WriteTask)>>,
    errors: Vec<SettingsError>,
    global_path: Option<String>,
    project_path: Option<String>,
}

impl SettingsManager {
    #[allow(clippy::too_many_arguments)] // 1:1 port of the upstream constructor surface
    fn new(
        storage: Arc<dyn SettingsStorage>,
        initial_global: SettingsMap,
        initial_project: SettingsMap,
        global_load_error: Option<String>,
        project_load_error: Option<String>,
        initial_errors: Vec<SettingsError>,
        project_trusted: bool,
        global_path: Option<String>,
        project_path: Option<String>,
    ) -> Self {
        let global_load_error = global_load_error.map(|e| {
            SettingsError::new(SettingsScope::Global, e.clone(), global_path.clone())
        });
        let project_load_error = project_load_error.map(|e| {
            SettingsError::new(SettingsScope::Project, e.clone(), project_path.clone())
        });
        let mut settings = initial_global.clone();
        deep_merge(&mut settings, &initial_project);
        Self {
            storage,
            global_settings: initial_global,
            project_settings: initial_project,
            settings,
            project_trusted,
            modified_fields: HashSet::new(),
            modified_nested: HashMap::new(),
            modified_project_fields: HashSet::new(),
            modified_project_nested: HashMap::new(),
            global_load_error,
            project_load_error,
            queue: Mutex::new(VecDeque::new()),
            errors: initial_errors,
            global_path,
            project_path,
        }
    }

    /// Load from files at `<agent_dir>/settings.json` and `<cwd>/.pi/settings.json`.
    pub fn create(cwd: &str, agent_dir: &str, options: SettingsManagerCreateOptions) -> Self {
        let storage = Arc::new(FileSettingsStorage::new(cwd, agent_dir));
        Self::from_storage_with_paths(storage, options, Some(agent_dir.to_string()), Some(cwd.to_string()))
    }

    /// Load from an arbitrary storage backend.
    pub fn from_storage(storage: Box<dyn SettingsStorage>) -> Self {
        Self::from_storage_with_paths(Arc::from(storage), SettingsManagerCreateOptions::default(), None, None)
    }

    /// In-memory manager; `settings` are migrated and seeded as the global scope.
    pub fn in_memory(settings: SettingsMap) -> Self {
        let storage = InMemorySettingsStorage::new();
        let mut migrated = settings.clone();
        migrate_settings(&mut migrated);
        let json = serde_json::to_string_pretty(&Value::Object(migrated.into_iter().collect()))
            .expect("settings serialize");
        storage.with_lock(SettingsScope::Global, &mut |_| Some(json.clone()));
        Self::from_storage_with_paths(
            Arc::new(storage),
            SettingsManagerCreateOptions::default(),
            None,
            None,
        )
    }

    /// Storage accessor (needed by the in-memory seam tests).
    pub fn storage(&self) -> &Arc<dyn SettingsStorage> {
        &self.storage
    }

    fn from_storage_with_paths(
        storage: Arc<dyn SettingsStorage>,
        options: SettingsManagerCreateOptions,
        agent_dir: Option<String>,
        cwd: Option<String>,
    ) -> Self {
        let project_trusted = options.project_trusted;
        let (global_settings, global_err) =
            Self::try_load_from_storage(storage.as_ref(), SettingsScope::Global, true);
        let (project_settings, project_err) =
            Self::try_load_from_storage(storage.as_ref(), SettingsScope::Project, project_trusted);
        let global_path = agent_dir.map(|d| format!("{d}/settings.json"));
        let project_path = cwd.map(|d| format!("{d}/{CONFIG_DIR_NAME}/settings.json"));
        let mut initial_errors = Vec::new();
        if let Some(e) = &global_err {
            initial_errors.push(SettingsError::new(SettingsScope::Global, e.clone(), global_path.clone()));
        }
        if let Some(e) = &project_err {
            initial_errors.push(SettingsError::new(SettingsScope::Project, e.clone(), project_path.clone()));
        }
        Self::new(
            storage,
            global_settings,
            project_settings,
            global_err,
            project_err,
            initial_errors,
            project_trusted,
            global_path,
            project_path,
        )
    }

    fn load_from_storage(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> Result<SettingsMap, String> {
        if scope == SettingsScope::Project && !project_trusted {
            return Ok(SettingsMap::new());
        }
        let mut content: Option<String> = None;
        {
            let mut capture = |current: Option<&str>| {
                content = current.map(|c| c.to_string());
                None
            };
            storage.with_lock(scope, &mut capture);
        }
        let Some(content) = content else { return Ok(SettingsMap::new()) };
        let content = strip_bom(&content);
        Self::parse_settings_map(content)
    }

    fn try_load_from_storage(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> (SettingsMap, Option<String>) {
        match Self::load_from_storage(storage, scope, project_trusted) {
            Ok(settings) => (settings, None),
            Err(e) => (SettingsMap::new(), Some(e)),
        }
    }

    /// Parse JSON (after BOM strip) and migrate; a non-object root yields an
    /// empty map (mirrors the upstream cast path).
    pub fn parse_settings_map(content: &str) -> Result<SettingsMap, String> {
        let value: Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
        let mut map: SettingsMap = match value {
            Value::Object(o) => o.into_iter().collect(),
            _ => return Ok(SettingsMap::new()),
        };
        migrate_settings(&mut map);
        Ok(map)
    }

    fn merged(&self) -> SettingsMap {
        let mut merged = self.global_settings.clone();
        deep_merge(&mut merged, &self.project_settings);
        merged
    }

    // ---- modified-field tracking (upstream markModified/clearModifiedScope) ----

    fn mark_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_nested
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    fn mark_project_modified(&mut self, field: &str, nested_key: Option<&str>) {
        self.modified_project_fields.insert(field.to_string());
        if let Some(nested_key) = nested_key {
            self.modified_project_nested
                .entry(field.to_string())
                .or_default()
                .insert(nested_key.to_string());
        }
    }

    fn clear_modified_scope(&mut self, scope: SettingsScope) {
        match scope {
            SettingsScope::Global => {
                self.modified_fields.clear();
                self.modified_nested.clear();
            }
            SettingsScope::Project => {
                self.modified_project_fields.clear();
                self.modified_project_nested.clear();
            }
        }
    }

    fn assert_project_trusted_for_write(&self) -> Result<(), String> {
        if self.project_trusted {
            Ok(())
        } else {
            Err("Project is not trusted; refusing to write project settings".to_string())
        }
    }

    fn record_error(&mut self, scope: SettingsScope, error: String) {
        let path = match scope {
            SettingsScope::Global => self.global_path.clone(),
            SettingsScope::Project => self.project_path.clone(),
        };
        self.errors.push(SettingsError::new(scope, error, path));
    }

    fn enqueue_write(&self, scope: SettingsScope, task: WriteTask) {
        self.queue.lock().unwrap().push_back((scope, task));
    }

    /// Merge only the snapshot's modified fields over the *current* file
    /// content (the heart of "preserve externally added settings").
    fn persist_scoped(
        storage: &dyn SettingsStorage,
        scope: SettingsScope,
        snapshot: &SettingsMap,
        modified_fields: &HashSet<String>,
        modified_nested: &HashMap<String, HashSet<String>>,
    ) {
        storage.with_lock(scope, &mut |current| {
            let mut merged_settings: SettingsMap = match current {
                Some(content) => {
                    let content = strip_bom(content);
                    // Migration applies to whatever is on disk (upstream
                    // `migrateSettings(JSON.parse(stripBom(current)))`).
                    Self::parse_settings_map(content).unwrap_or_default()
                }
                None => SettingsMap::new(),
            };
            for field in modified_fields {
                let Some(value) = snapshot.get(field) else {
                    // Removal: upstream sets the key to undefined, which
                    // JSON.stringify drops from the persisted object.
                    merged_settings.shift_remove(field);
                    continue;
                };
                if let Some(nested_keys) = modified_nested.get(field) {
                    if value.is_object() {
                        let base_nested: SettingsMap = merged_settings
                            .get(field)
                            .and_then(|v| v.as_object())
                            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                            .unwrap_or_default();
                        let in_memory_nested = value.as_object().unwrap();
                        let mut merged_nested = base_nested;
                        for nested_key in nested_keys {
                            merged_nested.insert(
                                nested_key.clone(),
                                in_memory_nested
                                    .get(nested_key)
                                    .cloned()
                                    .unwrap_or(Value::Null),
                            );
                        }
                        merged_settings
                            .insert(field.clone(), Value::Object(merged_nested.into_iter().collect()));
                    } else {
                        merged_settings.insert(field.clone(), value.clone());
                    }
                } else {
                    merged_settings.insert(field.clone(), value.clone());
                }
            }
            Some(
                serde_json::to_string_pretty(&Value::Object(merged_settings.into_iter().collect()))
                    .expect("settings serialize"),
            )
        });
    }

    fn save(&mut self) {
        self.settings = self.merged();
        if self.global_load_error.is_some() {
            return;
        }
        let snapshot = self.global_settings.clone();
        let fields = self.modified_fields.clone();
        let nested = self.modified_nested.clone();
        let storage = Arc::clone(&self.storage);
        self.enqueue_write(SettingsScope::Global, Box::new(move || {
            Self::persist_scoped(
                storage.as_ref(),
                SettingsScope::Global,
                &snapshot,
                &fields,
                &nested,
            );
        }));
    }

    fn save_project(&mut self) {
        if let Err(e) = self.assert_project_trusted_for_write() {
            panic!("{e}");
        }
        self.settings = self.merged();
        if self.project_load_error.is_some() {
            return;
        }
        let snapshot = self.project_settings.clone();
        let fields = self.modified_project_fields.clone();
        let nested = self.modified_project_nested.clone();
        let storage = Arc::clone(&self.storage);
        self.enqueue_write(SettingsScope::Project, Box::new(move || {
            Self::persist_scoped(
                storage.as_ref(),
                SettingsScope::Project,
                &snapshot,
                &fields,
                &nested,
            );
        }));
    }

    fn update_project_settings(&mut self, field: &str, update: impl FnOnce(&mut SettingsMap)) {
        if let Err(e) = self.assert_project_trusted_for_write() {
            panic!("{e}");
        }
        let mut project_settings = self.project_settings.clone();
        update(&mut project_settings);
        self.mark_project_modified(field, None);
        self.project_settings = project_settings;
        self.save_project();
    }

    /// Run the pending write queue; setter-side project trust is asserted here
    /// again, mirroring upstream `enqueueWrite`.
    pub async fn flush(&mut self) {
        let tasks: Vec<(SettingsScope, WriteTask)> = {
            let mut q = self.queue.lock().unwrap();
            q.drain(..).collect()
        };
        for (scope, task) in tasks {
            if scope == SettingsScope::Project {
                if let Err(e) = self.assert_project_trusted_for_write() {
                    self.record_error(scope, e);
                    continue;
                }
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
            if let Err(e) = result {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown write failure".to_string()
                };
                self.record_error(scope, msg);
                continue;
            }
            self.clear_modified_scope(scope);
        }
    }

    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    pub fn get_global_settings(&self) -> SettingsMap {
        self.global_settings.clone()
    }

    pub fn get_project_settings(&self) -> SettingsMap {
        self.project_settings.clone()
    }

    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.modified_project_fields.clear();
        self.modified_project_nested.clear();

        if !trusted {
            self.project_settings = SettingsMap::new();
            self.project_load_error = None;
            self.settings = self.merged();
            return;
        }

        let (settings, err) =
            Self::try_load_from_storage(self.storage.as_ref(), SettingsScope::Project, true);
        self.project_settings = settings;
        self.project_load_error = err.clone().map(|e| {
            SettingsError::new(SettingsScope::Project, e, self.project_path.clone())
        });
        if let Some(e) = err {
            self.record_error(SettingsScope::Project, e);
        }
        self.settings = self.merged();
    }

    pub async fn reload(&mut self) {
        self.flush().await;
        let (global, global_err) =
            Self::try_load_from_storage(self.storage.as_ref(), SettingsScope::Global, true);
        match global_err {
            None => {
                self.global_settings = global;
                self.global_load_error = None;
            }
            Some(e) => {
                self.global_load_error =
                    Some(SettingsError::new(SettingsScope::Global, e.clone(), self.global_path.clone()));
                self.record_error(SettingsScope::Global, e);
            }
        }
        self.modified_fields.clear();
        self.modified_nested.clear();
        self.modified_project_fields.clear();
        self.modified_project_nested.clear();

        let (project, project_err) = Self::try_load_from_storage(
            self.storage.as_ref(),
            SettingsScope::Project,
            self.project_trusted,
        );
        match project_err {
            None => {
                self.project_settings = project;
                self.project_load_error = None;
            }
            Some(e) => {
                self.project_load_error =
                    Some(SettingsError::new(SettingsScope::Project, e.clone(), self.project_path.clone()));
                self.record_error(SettingsScope::Project, e);
            }
        }
        self.settings = self.merged();
    }

    pub fn apply_overrides(&mut self, overrides: &SettingsMap) {
        deep_merge(&mut self.settings, overrides);
    }

    // ---- internal typed access helpers ------------------------------------

    fn g(&self, key: &str) -> Option<&Value> {
        self.settings.get(key)
    }

    fn g_bool(&self, key: &str) -> Option<bool> {
        self.g(key).and_then(|v| v.as_bool())
    }

    fn g_str(&self, key: &str) -> Option<&str> {
        self.g(key).and_then(|v| v.as_str())
    }

    fn g_u64(&self, key: &str) -> Option<u64> {
        self.g(key).and_then(|v| v.as_u64())
    }

    fn g_array_str(&self, key: &str) -> Option<Vec<String>> {
        self.g(key).and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect()
        })
    }

    fn g_nested(&self, key: &str, nested_key: &str) -> Option<&Value> {
        self.g(key).and_then(|v| v.get(nested_key))
    }

    fn g_nested_bool(&self, key: &str, nested_key: &str) -> Option<bool> {
        self.g_nested(key, nested_key).and_then(|v| v.as_bool())
    }

    fn g_nested_u64(&self, key: &str, nested_key: &str) -> Option<u64> {
        self.g_nested(key, nested_key).and_then(|v| v.as_u64())
    }

    fn set_global(&mut self, field: &str, value: Value) {
        self.global_settings.insert(field.to_string(), value);
        self.mark_modified(field, None);
        self.save();
    }

    fn ensure_global_object(&mut self, field: &str) {
        self.global_settings
            .entry(field.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }

    fn set_global_nested(&mut self, field: &str, nested_key: &str, value: Value) {
        self.ensure_global_object(field);
        self.global_settings[field]
            .as_object_mut()
            .expect("object just ensured")
            .insert(nested_key.to_string(), value);
        self.mark_modified(field, Some(nested_key));
        self.save();
    }

    // ---- accessors (ported 1:1 from the upstream SettingsManager surface) ----

    pub fn get_last_changelog_version(&self) -> Option<&str> {
        self.g_str("lastChangelogVersion")
    }

    pub fn set_last_changelog_version(&mut self, version: String) {
        self.set_global("lastChangelogVersion", Value::String(version));
    }

    /// sessionDir with `~` expansion (normalizePath).
    pub fn get_session_dir(&self) -> Option<String> {
        self.g_str("sessionDir").map(config::expand_tilde_path)
    }

    pub fn get_default_provider(&self) -> Option<&str> {
        self.g_str("defaultProvider")
    }

    pub fn get_default_model(&self) -> Option<&str> {
        self.g_str("defaultModel")
    }

    pub fn set_default_provider(&mut self, provider: String) {
        self.set_global("defaultProvider", Value::String(provider));
    }

    pub fn set_default_model(&mut self, model: String) {
        self.set_global("defaultModel", Value::String(model));
    }

    pub fn set_default_model_and_provider(&mut self, provider: String, model: String) {
        self.set_global("defaultProvider", Value::String(provider));
        self.set_global("defaultModel", Value::String(model));
    }

    pub fn get_steering_mode(&self) -> &str {
        self.g_str("steeringMode").unwrap_or("one-at-a-time")
    }

    pub fn set_steering_mode(&mut self, mode: &str) {
        self.set_global("steeringMode", Value::String(mode.to_string()));
    }

    pub fn get_follow_up_mode(&self) -> &str {
        self.g_str("followUpMode").unwrap_or("one-at-a-time")
    }

    pub fn set_follow_up_mode(&mut self, mode: &str) {
        self.set_global("followUpMode", Value::String(mode.to_string()));
    }

    /// Raw theme value (slash-separated automatic themes are kept raw).
    pub fn get_theme_setting(&self) -> Option<&str> {
        self.g_str("theme")
    }

    /// Fixed theme name; None when the raw value is a slash-separated auto theme.
    pub fn get_theme(&self) -> Option<String> {
        self.get_theme_setting()
            .filter(|t| !t.contains('/'))
            .map(|t| t.to_string())
    }

    pub fn set_theme(&mut self, theme: String) {
        self.set_global("theme", Value::String(theme));
    }

    pub fn get_default_thinking_level(&self) -> Option<&str> {
        self.g_str("defaultThinkingLevel")
    }

    pub fn set_default_thinking_level(&mut self, level: &str) {
        self.set_global("defaultThinkingLevel", Value::String(level.to_string()));
    }

    pub fn get_model_thinking_level(&self, provider: &str, model_id: &str) -> Option<&str> {
        self.g("modelThinkingLevels")
            .and_then(|v| v.get(format!("{provider}/{model_id}")))
            .and_then(|v| v.as_str())
    }

    pub fn get_all_model_thinking_levels(&self) -> SettingsMap {
        self.g("modelThinkingLevels")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    pub fn set_model_thinking_level(&mut self, provider: &str, model_id: &str, level: &str) {
        if !self.global_settings.contains_key("modelThinkingLevels") {
            self.global_settings
                .insert("modelThinkingLevels".to_string(), Value::Object(serde_json::Map::new()));
        }
        self.global_settings["modelThinkingLevels"]
            .as_object_mut()
            .expect("object just ensured")
            .insert(format!("{provider}/{model_id}"), Value::String(level.to_string()));
        self.mark_modified("modelThinkingLevels", None);
        self.save();
    }

    pub fn remove_model_thinking_level(&mut self, provider: &str, model_id: &str) {
        let Some(obj) = self.global_settings.get_mut("modelThinkingLevels").and_then(|v| v.as_object_mut()) else {
            return;
        };
        obj.shift_remove(&format!("{provider}/{model_id}"));
        if obj.is_empty() {
            self.global_settings.shift_remove("modelThinkingLevels");
        }
        self.mark_modified("modelThinkingLevels", None);
        self.save();
    }

    pub fn get_transport(&self) -> &str {
        self.g_str("transport").unwrap_or("auto")
    }

    pub fn set_transport(&mut self, transport: &str) {
        self.set_global("transport", Value::String(transport.to_string()));
    }

    pub fn get_compaction_enabled(&self) -> bool {
        self.g_nested_bool("compaction", "enabled").unwrap_or(true)
    }

    pub fn set_compaction_enabled(&mut self, enabled: bool) {
        self.set_global_nested("compaction", "enabled", Value::Bool(enabled));
    }

    pub fn get_compaction_reserve_tokens(&self) -> u64 {
        self.g_nested_u64("compaction", "reserveTokens").unwrap_or(16384)
    }

    pub fn get_compaction_keep_recent_tokens(&self) -> u64 {
        self.g_nested_u64("compaction", "keepRecentTokens").unwrap_or(20000)
    }

    pub fn get_compaction_settings(&self) -> (bool, u64, u64) {
        (
            self.get_compaction_enabled(),
            self.get_compaction_reserve_tokens(),
            self.get_compaction_keep_recent_tokens(),
        )
    }

    pub fn get_branch_summary_settings(&self) -> (u64, bool) {
        (
            self.g_nested_u64("branchSummary", "reserveTokens").unwrap_or(16384),
            self.g_nested_bool("branchSummary", "skipPrompt").unwrap_or(false),
        )
    }

    pub fn get_branch_summary_skip_prompt(&self) -> bool {
        self.g_nested_bool("branchSummary", "skipPrompt").unwrap_or(false)
    }

    pub fn get_retry_enabled(&self) -> bool {
        self.g_nested_bool("retry", "enabled").unwrap_or(true)
    }

    pub fn set_retry_enabled(&mut self, enabled: bool) {
        self.set_global_nested("retry", "enabled", Value::Bool(enabled));
    }

    pub fn get_retry_settings(&self) -> (bool, u64, u64) {
        (
            self.get_retry_enabled(),
            self.g_nested_u64("retry", "maxRetries").unwrap_or(3),
            self.g_nested_u64("retry", "baseDelayMs").unwrap_or(2000),
        )
    }

    fn parse_timeout_setting(&self, key: &str, setting_name: &str) -> Result<Option<u64>, String> {
        match self.g(key) {
            None => Ok(None),
            Some(value) => match parse_http_idle_timeout_ms(value) {
                Some(timeout) => Ok(Some(timeout)),
                None => Err(format!("Invalid {setting_name} setting: {value}")),
            },
        }
    }

    pub fn get_http_idle_timeout_ms(&self) -> Result<u64, String> {
        Ok(self
            .parse_timeout_setting("httpIdleTimeoutMs", "httpIdleTimeoutMs")?
            .unwrap_or(300_000))
    }

    pub fn set_http_idle_timeout_ms(&mut self, timeout_ms: f64) -> Result<(), String> {
        if !timeout_ms.is_finite() || timeout_ms < 0.0 {
            return Err(format!("Invalid httpIdleTimeoutMs setting: {timeout_ms}"));
        }
        self.set_global("httpIdleTimeoutMs", json!(timeout_ms.floor() as u64));
        Ok(())
    }

    pub fn get_provider_retry_settings(&self) -> (Option<u64>, Option<u64>, u64) {
        let provider = self.g_nested("retry", "provider").and_then(|v| v.as_object());
        (
            provider.and_then(|p| p.get("timeoutMs").and_then(|v| v.as_u64())),
            provider.and_then(|p| p.get("maxRetries").and_then(|v| v.as_u64())),
            provider
                .and_then(|p| p.get("maxRetryDelayMs").and_then(|v| v.as_u64()))
                .unwrap_or(60000),
        )
    }

    pub fn get_websocket_connect_timeout_ms(&self) -> Result<Option<u64>, String> {
        self.parse_timeout_setting("websocketConnectTimeoutMs", "websocketConnectTimeoutMs")
    }

    pub fn get_hide_thinking_block(&self) -> bool {
        self.g_bool("hideThinkingBlock").unwrap_or(false)
    }

    pub fn get_show_cache_miss_notices(&self) -> bool {
        self.g_bool("showCacheMissNotices").unwrap_or(false)
    }

    pub fn get_external_editor_command(&self) -> String {
        let configured = self.g_str("externalEditor").unwrap_or("");
        if !configured.trim().is_empty() {
            return configured.to_string();
        }
        if let Ok(visual) = std::env::var("VISUAL") {
            if !visual.is_empty() {
                return visual;
            }
        }
        if let Ok(editor) = std::env::var("EDITOR") {
            if !editor.is_empty() {
                return editor;
            }
        }
        if cfg!(windows) {
            "notepad".to_string()
        } else {
            "nano".to_string()
        }
    }

    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.set_global("hideThinkingBlock", Value::Bool(hide));
    }

    pub fn set_show_cache_miss_notices(&mut self, show: bool) {
        self.set_global("showCacheMissNotices", Value::Bool(show));
    }

    /// shellPath with `~` expansion (normalizePath).
    pub fn get_shell_path(&self) -> Option<String> {
        self.g_str("shellPath").map(config::expand_tilde_path)
    }

    pub fn set_shell_path(&mut self, path: Option<String>) {
        match path {
            Some(p) => self.set_global("shellPath", Value::String(p)),
            None => {
                self.global_settings.shift_remove("shellPath");
                self.mark_modified("shellPath", None);
                self.save();
            }
        }
    }

    pub fn get_quiet_startup(&self) -> bool {
        self.g_bool("quietStartup").unwrap_or(false)
    }

    pub fn set_quiet_startup(&mut self, quiet: bool) {
        self.set_global("quietStartup", Value::Bool(quiet));
    }

    /// Read from *global* scope only; anything other than always/never is ask.
    pub fn get_default_project_trust(&self) -> &str {
        match self.global_settings.get("defaultProjectTrust").and_then(|v| v.as_str()) {
            Some("always") => "always",
            Some("never") => "never",
            _ => "ask",
        }
    }

    pub fn set_default_project_trust(&mut self, trust: &str) {
        self.set_global("defaultProjectTrust", Value::String(trust.to_string()));
    }

    pub fn get_shell_command_prefix(&self) -> Option<&str> {
        self.g_str("shellCommandPrefix")
    }

    pub fn set_shell_command_prefix(&mut self, prefix: Option<String>) {
        match prefix {
            Some(p) => self.set_global("shellCommandPrefix", Value::String(p)),
            None => {
                self.global_settings.shift_remove("shellCommandPrefix");
                self.mark_modified("shellCommandPrefix", None);
                self.save();
            }
        }
    }

    pub fn get_npm_command(&self) -> Option<Vec<String>> {
        self.g_array_str("npmCommand")
    }

    pub fn set_npm_command(&mut self, command: Option<Vec<String>>) {
        match command {
            Some(c) => self.set_global(
                "npmCommand",
                Value::Array(c.into_iter().map(Value::String).collect()),
            ),
            None => {
                self.global_settings.shift_remove("npmCommand");
                self.mark_modified("npmCommand", None);
                self.save();
            }
        }
    }

    pub fn get_collapse_changelog(&self) -> bool {
        self.g_bool("collapseChangelog").unwrap_or(false)
    }

    pub fn set_collapse_changelog(&mut self, collapse: bool) {
        self.set_global("collapseChangelog", Value::Bool(collapse));
    }

    pub fn get_enable_install_telemetry(&self) -> bool {
        self.g_bool("enableInstallTelemetry").unwrap_or(true)
    }

    pub fn set_enable_install_telemetry(&mut self, enabled: bool) {
        self.set_global("enableInstallTelemetry", Value::Bool(enabled));
    }

    pub fn get_enable_analytics(&self) -> bool {
        self.g_bool("enableAnalytics").unwrap_or(false)
    }

    pub fn get_tracking_id(&self) -> Option<&str> {
        self.g_str("trackingId")
    }

    pub fn set_enable_analytics(&mut self, enabled: bool) {
        self.global_settings
            .insert("enableAnalytics".to_string(), Value::Bool(enabled));
        self.mark_modified("enableAnalytics", None);
        if enabled && self.global_settings.get("trackingId").is_none() {
            let id = uuid::Uuid::new_v4().to_string();
            self.global_settings.insert("trackingId".to_string(), Value::String(id));
            self.mark_modified("trackingId", None);
        }
        self.save();
    }

    pub fn get_packages(&self) -> Vec<PackageSource> {
        self.g("packages")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    pub fn set_packages(&mut self, packages: Vec<PackageSource>) {
        let value = serde_json::to_value(packages).expect("packages serialize");
        self.set_global("packages", value);
    }

    pub fn set_project_packages(&mut self, packages: Vec<PackageSource>) {
        let value = serde_json::to_value(packages).expect("packages serialize");
        self.update_project_settings("packages", |settings| {
            settings.insert("packages".to_string(), value);
        });
    }

    pub fn get_extension_paths(&self) -> Vec<String> {
        self.g_array_str("extensions").unwrap_or_default()
    }

    pub fn set_extension_paths(&mut self, paths: Vec<String>) {
        self.set_global(
            "extensions",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    pub fn set_project_extension_paths(&mut self, paths: Vec<String>) {
        let value = Value::Array(paths.into_iter().map(Value::String).collect());
        self.update_project_settings("extensions", |settings| {
            settings.insert("extensions".to_string(), value);
        });
    }

    pub fn get_skill_paths(&self) -> Vec<String> {
        self.g_array_str("skills").unwrap_or_default()
    }

    pub fn set_skill_paths(&mut self, paths: Vec<String>) {
        self.set_global(
            "skills",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    pub fn set_project_skill_paths(&mut self, paths: Vec<String>) {
        let value = Value::Array(paths.into_iter().map(Value::String).collect());
        self.update_project_settings("skills", |settings| {
            settings.insert("skills".to_string(), value);
        });
    }

    pub fn get_prompt_template_paths(&self) -> Vec<String> {
        self.g_array_str("prompts").unwrap_or_default()
    }

    pub fn set_prompt_template_paths(&mut self, paths: Vec<String>) {
        self.set_global(
            "prompts",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    pub fn set_project_prompt_template_paths(&mut self, paths: Vec<String>) {
        let value = Value::Array(paths.into_iter().map(Value::String).collect());
        self.update_project_settings("prompts", |settings| {
            settings.insert("prompts".to_string(), value);
        });
    }

    pub fn get_theme_paths(&self) -> Vec<String> {
        self.g_array_str("themes").unwrap_or_default()
    }

    pub fn set_theme_paths(&mut self, paths: Vec<String>) {
        self.set_global(
            "themes",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    pub fn set_project_theme_paths(&mut self, paths: Vec<String>) {
        let value = Value::Array(paths.into_iter().map(Value::String).collect());
        self.update_project_settings("themes", |settings| {
            settings.insert("themes".to_string(), value);
        });
    }

    pub fn get_enable_skill_commands(&self) -> bool {
        self.g_bool("enableSkillCommands").unwrap_or(true)
    }

    pub fn set_enable_skill_commands(&mut self, enabled: bool) {
        self.set_global("enableSkillCommands", Value::Bool(enabled));
    }

    pub fn get_thinking_budgets(&self) -> Option<SettingsMap> {
        self.g("thinkingBudgets").and_then(|v| v.as_object()).map(|o| {
            o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        })
    }

    pub fn get_show_images(&self) -> bool {
        self.g_nested_bool("terminal", "showImages").unwrap_or(true)
    }

    pub fn set_show_images(&mut self, show: bool) {
        self.set_global_nested("terminal", "showImages", Value::Bool(show));
    }

    pub fn get_image_width_cells(&self) -> u64 {
        match self.g_nested("terminal", "imageWidthCells").and_then(|v| v.as_u64()) {
            Some(width) => width.max(1),
            None => 60,
        }
    }

    pub fn set_image_width_cells(&mut self, width: f64) {
        let width = (width.floor() as u64).max(1);
        self.set_global_nested("terminal", "imageWidthCells", json!(width));
    }

    pub fn get_clear_on_shrink(&self) -> bool {
        if let Some(v) = self.g_nested_bool("terminal", "clearOnShrink") {
            return v;
        }
        config::env("PI_CLEAR_ON_SHRINK").as_deref() == Some("1")
    }

    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.set_global_nested("terminal", "clearOnShrink", Value::Bool(enabled));
    }

    pub fn get_show_terminal_progress(&self) -> bool {
        self.g_nested_bool("terminal", "showTerminalProgress").unwrap_or(false)
    }

    pub fn set_show_terminal_progress(&mut self, enabled: bool) {
        self.set_global_nested("terminal", "showTerminalProgress", Value::Bool(enabled));
    }

    pub fn get_tui_mode(&self) -> &str {
        if self.g_str("tuiMode") == Some("fullscreen") {
            "fullscreen"
        } else {
            "regular"
        }
    }

    pub fn set_tui_mode(&mut self, mode: &str) {
        self.set_global("tuiMode", Value::String(mode.to_string()));
    }

    pub fn get_fullscreen_exit_output(&self) -> &str {
        if self.g_str("fullscreenExitOutput") == Some("resume-hint") {
            "resume-hint"
        } else {
            "transcript"
        }
    }

    pub fn set_fullscreen_exit_output(&mut self, output: &str) {
        self.set_global("fullscreenExitOutput", Value::String(output.to_string()));
    }

    pub fn get_fullscreen_scrollbar(&self) -> &str {
        match self.g_str("fullscreenScrollbar") {
            Some("always") => "always",
            Some("hidden") => "hidden",
            _ => "auto",
        }
    }

    pub fn set_fullscreen_scrollbar(&mut self, mode: &str) {
        self.set_global("fullscreenScrollbar", Value::String(mode.to_string()));
    }

    pub fn get_image_auto_resize(&self) -> bool {
        self.g_nested_bool("images", "autoResize").unwrap_or(true)
    }

    pub fn set_image_auto_resize(&mut self, enabled: bool) {
        self.set_global_nested("images", "autoResize", Value::Bool(enabled));
    }

    pub fn get_block_images(&self) -> bool {
        self.g_nested_bool("images", "blockImages").unwrap_or(false)
    }

    pub fn set_block_images(&mut self, blocked: bool) {
        self.set_global_nested("images", "blockImages", Value::Bool(blocked));
    }

    pub fn get_enabled_models(&self) -> Option<Vec<String>> {
        self.g_array_str("enabledModels")
    }

    pub fn get_default_tools(&self) -> Option<Vec<String>> {
        self.g_array_str("defaultTools")
    }

    pub fn set_enabled_models(&mut self, patterns: Option<Vec<String>>) {
        match patterns {
            Some(p) => self.set_global(
                "enabledModels",
                Value::Array(p.into_iter().map(Value::String).collect()),
            ),
            None => {
                self.global_settings.shift_remove("enabledModels");
                self.mark_modified("enabledModels", None);
                self.save();
            }
        }
    }

    pub fn get_double_escape_action(&self) -> &str {
        self.g_str("doubleEscapeAction").unwrap_or("tree")
    }

    pub fn set_double_escape_action(&mut self, action: &str) {
        self.set_global("doubleEscapeAction", Value::String(action.to_string()));
    }

    pub fn get_tree_filter_mode(&self) -> &str {
        match self.g_str("treeFilterMode") {
            Some(m) if matches!(m, "default" | "no-tools" | "user-only" | "labeled-only" | "all") => m,
            _ => "default",
        }
    }

    pub fn set_tree_filter_mode(&mut self, mode: &str) {
        self.set_global("treeFilterMode", Value::String(mode.to_string()));
    }

    pub fn get_show_hardware_cursor(&self) -> bool {
        if let Some(v) = self.g_bool("showHardwareCursor") {
            return v;
        }
        config::env("PI_HARDWARE_CURSOR").as_deref() == Some("1")
    }

    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.set_global("showHardwareCursor", Value::Bool(enabled));
    }

    pub fn get_editor_padding_x(&self) -> u64 {
        self.g_u64("editorPaddingX").unwrap_or(0)
    }

    pub fn set_editor_padding_x(&mut self, padding: f64) {
        let clamped = (padding.floor() as i64).clamp(0, 3) as u64;
        self.set_global("editorPaddingX", json!(clamped));
    }

    pub fn get_output_pad(&self) -> u64 {
        if self.g_u64("outputPad") == Some(0) {
            0
        } else {
            1
        }
    }

    pub fn set_output_pad(&mut self, padding: u64) {
        let padding = if padding == 0 { 0 } else { 1 };
        self.set_global("outputPad", json!(padding));
    }

    pub fn get_autocomplete_max_visible(&self) -> u64 {
        self.g_u64("autocompleteMaxVisible").unwrap_or(5)
    }

    pub fn set_autocomplete_max_visible(&mut self, max_visible: f64) {
        let clamped = (max_visible.floor() as i64).clamp(3, 20) as u64;
        self.set_global("autocompleteMaxVisible", json!(clamped));
    }

    pub fn get_code_block_indent(&self) -> &str {
        self.g_nested("markdown", "codeBlockIndent")
            .and_then(|v| v.as_str())
            .unwrap_or("  ")
    }

    pub fn get_mermaid_rendering_mode(&self) -> &str {
        match self.g_nested("markdown", "mermaid").and_then(|v| v.as_str()) {
            Some("off") => "off",
            Some("final") => "final",
            _ => "streaming",
        }
    }

    pub fn set_mermaid_rendering_mode(&mut self, mode: &str) {
        self.set_global_nested("markdown", "mermaid", Value::String(mode.to_string()));
    }

    pub fn get_warnings(&self) -> SettingsMap {
        self.g("warnings")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    pub fn set_warnings(&mut self, warnings: SettingsMap) {
        self.set_global("warnings", Value::Object(warnings.into_iter().collect()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> SettingsMap {
        serde_json::from_value(v).unwrap()
    }

    // ---- deep_merge ------------------------------------------------------

    #[test]
    fn deep_merge_adds_missing_keys() {
        let mut base = m(json!({ "a": 1 }));
        let overrides = m(json!({ "b": 2 }));
        deep_merge(&mut base, &overrides);
        assert_eq!(base, m(json!({ "a": 1, "b": 2 })));
    }

    #[test]
    fn deep_merge_nested_objects_merge_recursively() {
        let mut base = m(json!({ "compaction": { "enabled": true } }));
        let overrides =
            m(json!({ "compaction": { "reserveTokens": 10 } }));
        deep_merge(&mut base, &overrides);
        assert_eq!(
            base,
            m(json!({ "compaction": { "enabled": true, "reserveTokens": 10 } }))
        );
    }

    #[test]
    fn deep_merge_non_object_override_replaces() {
        let mut base = m(json!({ "a": { "x": 1 } }));
        let overrides = m(json!({ "a": 5 }));
        deep_merge(&mut base, &overrides);
        assert_eq!(base, m(json!({ "a": 5 })));
    }

    #[test]
    fn deep_merge_arrays_replace() {
        let mut base = m(json!({ "extensions": ["/one.ts"] }));
        let overrides = m(json!({ "extensions": ["/two.ts", "/three.ts"] }));
        deep_merge(&mut base, &overrides);
        assert_eq!(
            base,
            m(json!({ "extensions": ["/two.ts", "/three.ts"] }))
        );
    }

    // ---- migrate_settings ------------------------------------------------

    #[test]
    fn migrate_queue_mode_to_steering_mode() {
        let mut settings = m(json!({ "queueMode": "one-at-a-time" }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "steeringMode": "one-at-a-time" }))
        );
    }

    #[test]
    fn migrate_queue_mode_keeps_existing_steering_mode() {
        // Upstream: the migration only fires when the target key is absent;
        // otherwise the legacy key stays untouched.
        let mut settings =
            m(json!({ "steeringMode": "all", "queueMode": "one-at-a-time" }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "steeringMode": "all", "queueMode": "one-at-a-time" }))
        );
    }

    #[test]
    fn migrate_websockets_true_to_transport_websocket() {
        let mut settings = m(json!({ "websockets": true }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "transport": "websocket" }))
        );
    }

    #[test]
    fn migrate_websockets_false_to_transport_sse() {
        let mut settings = m(json!({ "websockets": false }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "transport": "sse" }))
        );
    }

    #[test]
    fn migrate_websockets_keeps_existing_transport() {
        // Upstream: same guard — legacy key stays when transport exists.
        let mut settings =
            m(json!({ "transport": "auto", "websockets": true }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "transport": "auto", "websockets": true }))
        );
    }

    #[test]
    fn migrate_skills_object_to_array() {
        let mut settings = m(json!({
            "skills": { "enableSkillCommands": false, "customDirectories": ["/a", "/b"] }
        }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({
                "skills": ["/a", "/b"],
                "enableSkillCommands": false
            }))
        );
    }

    #[test]
    fn migrate_skills_object_without_custom_directories_deletes_skills() {
        let mut settings =
            m(json!({ "skills": { "enableSkillCommands": false } }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "enableSkillCommands": false }))
        );
    }

    #[test]
    fn migrate_skills_array_is_untouched() {
        let mut settings = m(json!({ "skills": ["/a"] }));
        migrate_settings(&mut settings);
        assert_eq!(settings, m(json!({ "skills": ["/a"] })));
    }

    #[test]
    fn migrate_retry_max_delay_to_provider() {
        let mut settings = m(json!({ "retry": { "maxDelayMs": 5000 } }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "retry": { "provider": { "maxRetryDelayMs": 5000 } } }))
        );
    }

    #[test]
    fn migrate_retry_keeps_existing_provider_max_retry_delay() {
        let mut settings = m(json!({
            "retry": { "maxDelayMs": 5000, "provider": { "maxRetryDelayMs": 100 } }
        }));
        migrate_settings(&mut settings);
        assert_eq!(
            settings,
            m(json!({ "retry": { "provider": { "maxRetryDelayMs": 100 } } }))
        );
    }

    // ---- parse_http_idle_timeout_ms --------------------------------------

    #[test]
    fn timeout_parses_number() {
        assert_eq!(parse_http_idle_timeout_ms(&json!(300000)), Some(300000));
    }

    #[test]
    fn timeout_parses_string_disabled() {
        assert_eq!(parse_http_idle_timeout_ms(&json!("disabled")), Some(0));
        assert_eq!(parse_http_idle_timeout_ms(&json!("DISABLED")), Some(0));
    }

    #[test]
    fn timeout_empty_string_is_none() {
        assert_eq!(parse_http_idle_timeout_ms(&json!("")), None);
    }

    #[test]
    fn timeout_negative_or_non_finite_is_none() {
        assert_eq!(parse_http_idle_timeout_ms(&json!(-1)), None);
    }

    #[test]
    fn timeout_non_numeric_string_is_none() {
        assert_eq!(parse_http_idle_timeout_ms(&json!("abc")), None);
    }

    #[test]
    fn timeout_floors_float() {
        assert_eq!(parse_http_idle_timeout_ms(&json!(1.99)), Some(1));
    }

    #[test]
    fn timeout_parses_numeric_string() {
        assert_eq!(parse_http_idle_timeout_ms(&json!("5000")), Some(5000));
    }

    // ---- strip_bom --------------------------------------------------------

    #[test]
    fn strip_bom_removes_bom() {
        assert_eq!(strip_bom("\u{FEFF}{}\n"), "{}\n");
    }

    #[test]
    fn strip_bom_passthrough_without_bom() {
        assert_eq!(strip_bom("{}"), "{}");
    }
}
