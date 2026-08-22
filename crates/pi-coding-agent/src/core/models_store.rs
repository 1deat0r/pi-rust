//! Persistent model catalogs for the coding agent — port of
//! `packages/coding-agent/src/core/models-store.ts`.
//!
//! Two storages:
//! - `InMemoryCodingAgentModelsStore`: in-memory map (used for auth checks).
//! - `FileModelsStore`: locked JSON file at `models-store.json` next to
//!   `models.json`, holding a `Record<providerId, ModelsStoreEntry>` map,
//!   pretty-printed with 2-space indent (on-disk format parity).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use pi_ai::model::Model;
use pi_ai::models::{ModelsStore, ModelsStoreEntry};

/// On-disk shape of one catalog entry (upstream `ModelsStoreEntry`).
/// The pi-ai `ModelsStoreEntry` is not serializable, so this module carries
/// its own serde twin for the stored JSON (same field names/renames).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct StoredModelsEntry {
    pub models: Vec<Model>,
    #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    #[serde(rename = "checkedAt", skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl From<&StoredModelsEntry> for ModelsStoreEntry {
    fn from(entry: &StoredModelsEntry) -> Self {
        ModelsStoreEntry {
            models: entry.models.clone(),
            last_modified: entry.last_modified,
            checked_at: entry.checked_at,
            etag: entry.etag.clone(),
        }
    }
}

impl From<StoredModelsEntry> for ModelsStoreEntry {
    fn from(entry: StoredModelsEntry) -> Self {
        ModelsStoreEntry {
            models: entry.models,
            last_modified: entry.last_modified,
            checked_at: entry.checked_at,
            etag: entry.etag,
        }
    }
}

impl From<ModelsStoreEntry> for StoredModelsEntry {
    fn from(entry: ModelsStoreEntry) -> Self {
        StoredModelsEntry {
            models: entry.models,
            last_modified: entry.last_modified,
            checked_at: entry.checked_at,
            etag: entry.etag,
        }
    }
}

impl From<&ModelsStoreEntry> for StoredModelsEntry {
    fn from(entry: &ModelsStoreEntry) -> Self {
        StoredModelsEntry {
            models: entry.models.clone(),
            last_modified: entry.last_modified,
            checked_at: entry.checked_at,
            etag: entry.etag.clone(),
        }
    }
}

/// `Record<providerId, ModelsStoreEntry>` stored in models-store.json.
pub type StoredModels = BTreeMap<String, StoredModelsEntry>;

/// In-memory models store (upstream `InMemoryCodingAgentModelsStore`).
#[derive(Debug, Clone, Default)]
pub struct InMemoryCodingAgentModelsStore {
    entries: Arc<Mutex<BTreeMap<String, StoredModelsEntry>>>,
}

impl InMemoryCodingAgentModelsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ModelsStore for InMemoryCodingAgentModelsStore {
    fn read(&self, provider_id: &str) -> Option<ModelsStoreEntry> {
        self.entries
            .lock()
            .unwrap()
            .get(provider_id)
            .cloned()
            .map(ModelsStoreEntry::from)
    }
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry) {
        self.entries
            .lock()
            .unwrap()
            .insert(provider_id.to_string(), StoredModelsEntry::from(entry));
    }
    fn delete(&self, provider_id: &str) {
        self.entries.lock().unwrap().remove(provider_id);
    }
}

/// Acquire an exclusive advisory lock for the models store file. The lock is
/// a sibling `.lock` file created with `create_new` semantics; the upstream
/// `FileAuthStorageBackend` uses the same lock-file strategy.
fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let lock_path = path.with_extension("lock");
    let _guard = FileLockGuard::acquire(&lock_path);
    f()
}

struct FileLockGuard {
    path: PathBuf,
}

impl FileLockGuard {
    fn acquire(path: &Path) -> Self {
        // Retry briefly to mirror the upstream lock-acquire with retry.
        for _ in 0..200 {
            match fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => return Self { path: path.to_path_buf() },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        // Fall back to overwriting the lock if it is stale (mirrors the
        // upstream lock-acquire-with-retry timeout behavior).
        let _ = fs::write(path, "");
        Self { path: path.to_path_buf() }
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A file mtime in milliseconds (upstream `getFileRevision` uses mtimeMs).
pub fn file_revision(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let duration = mtime.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as u64)
}

/// Locked JSON-backed storage for dynamically refreshed provider catalogs
/// (upstream `FileModelsStore`).
///
/// Observable surface parity:
/// - default path `getAgentDir()/models-store.json`
/// - contents `{ "<providerId>": ModelsStoreEntry, ... }`
/// - 2-space pretty-printed writes
#[derive(Debug, Clone)]
pub struct FileModelsStore {
    path: PathBuf,
    cache: Arc<Mutex<CachedState>>,
}

#[derive(Debug, Clone, Default)]
struct CachedState {
    data: StoredModels,
    revision: Option<u64>,
}

impl FileModelsStore {
    pub fn new(path: PathBuf) -> Self {
        let state = CachedState::default();
        Self { path, cache: Arc::new(Mutex::new(state)) }
    }

    /// Default path: `getAgentDir()/models-store.json`.
    pub fn default_path() -> PathBuf {
        crate::config::get_agent_dir().join("models-store.json")
    }

    fn parse(content: &str) -> StoredModels {
        if content.trim().is_empty() {
            return StoredModels::new();
        }
        serde_json::from_str(content).unwrap_or_default()
    }

    fn read_latest(&self) -> StoredModels {
        let revision = file_revision(&self.path);
        let mut cache = self.cache.lock().unwrap();
        if revision.is_some() && revision == cache.revision {
            return cache.data.clone();
        }
        let data = match fs::read_to_string(&self.path) {
            Ok(content) => Self::parse(crate::core::settings::strip_bom(&content)),
            Err(_) => StoredModels::new(),
        };
        cache.data = data.clone();
        cache.revision = revision;
        data
    }

    /// Read the on-disk JSON into the internal entry shape.
    fn read_entry(&self, provider_id: &str) -> Option<ModelsStoreEntry> {
        self.read_latest().get(provider_id).cloned().map(ModelsStoreEntry::from)
    }

    fn write_locked(&self, f: impl FnOnce(&mut StoredModels)) {
        with_file_lock(&self.path, || {
            let mut data = self.read_latest();
            f(&mut data);
            let serialized = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".to_string());
            if let Some(parent) = self.path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(&self.path, serialized).expect("write models-store.json");
            let mut cache = self.cache.lock().unwrap();
            cache.data = data;
            cache.revision = file_revision(&self.path);
        });
    }
}

impl ModelsStore for FileModelsStore {
    fn read(&self, provider_id: &str) -> Option<ModelsStoreEntry> {
        self.read_entry(provider_id)
    }
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry) {
        self.write_locked(|data| {
            data.insert(provider_id.to_string(), StoredModelsEntry::from(entry));
        });
    }
    fn delete(&self, provider_id: &str) {
        self.write_locked(|data| {
            data.remove(provider_id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::model::Model;

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("pi-models-store-{tag}-{}", uuid::Uuid::new_v4()))
            .join("models-store.json")
    }

    fn entry_with_model() -> ModelsStoreEntry {
        let mut model = Model::new("demo-1", "Demo 1", "openai-responses", "demo");
        model.authenticated = true;
        ModelsStoreEntry {
            models: vec![model],
            last_modified: Some(1234),
            checked_at: Some(5678),
            etag: Some("abc".to_string()),
        }
    }

    #[test]
    fn in_memory_read_write_delete() {
        let store = InMemoryCodingAgentModelsStore::new();
        assert!(store.read("demo").is_none());
        let entry = entry_with_model();
        store.write("demo", &entry);
        let read = store.read("demo").unwrap();
        assert_eq!(read.models.len(), 1);
        assert_eq!(read.models[0].id, "demo-1");
        assert_eq!(read.etag.as_deref(), Some("abc"));
        store.delete("demo");
        assert!(store.read("demo").is_none());
    }

    #[test]
    fn file_store_persists_pretty_json() {
        let path = tmp_path("write");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = FileModelsStore::new(path.clone());
        store.write("demo", &entry_with_model());
        let content = std::fs::read_to_string(&path).unwrap();
        // Pretty-printed with 2-space indent.
        assert!(content.contains("\n  \"demo\": {"), "content: {content}");
        assert!(content.contains("\n    \"models\": ["), "content: {content}");
        // Round trip.
        let store2 = FileModelsStore::new(path.clone());
        let read = store2.read("demo").unwrap();
        assert_eq!(read.models[0].id, "demo-1");
        assert_eq!(read.checked_at, Some(5678));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn file_store_update_preserves_other_providers() {
        let path = tmp_path("multi");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = FileModelsStore::new(path.clone());
        store.write("demo", &entry_with_model());
        store.write("other", &entry_with_model());
        let store2 = FileModelsStore::new(path.clone());
        assert!(store2.read("demo").is_some());
        assert!(store2.read("other").is_some());
        store2.delete("demo");
        let store3 = FileModelsStore::new(path.clone());
        assert!(store3.read("demo").is_none());
        assert!(store3.read("other").is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn file_store_missing_file_yields_empty() {
        let path = tmp_path("missing");
        let store = FileModelsStore::new(path);
        assert!(store.read("demo").is_none());
    }

    #[test]
    fn file_store_reads_corrupt_file_as_empty() {
        let path = tmp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let store = FileModelsStore::new(path.clone());
        assert!(store.read("demo").is_none());
        // A subsequent write repairs the file.
        store.write("demo", &entry_with_model());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("demo"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn default_path_is_agent_dir_models_store() {
        let p = FileModelsStore::default_path();
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "models-store.json");
    }
}
