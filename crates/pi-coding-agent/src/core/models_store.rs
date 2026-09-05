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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

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
            .unwrap_or_else(|error| error.into_inner())
            .get(provider_id)
            .cloned()
            .map(ModelsStoreEntry::from)
    }
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(provider_id.to_string(), StoredModelsEntry::from(entry));
    }
    fn delete(&self, provider_id: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(provider_id);
    }
}

/// Acquire an exclusive advisory lock for the models store file. The lock is
/// a sibling `.lock` file created with `create_new` semantics; the upstream
/// `FileAuthStorageBackend` uses the same lock-file strategy.
fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    // `proper-lockfile` (used by upstream FileAuthStorageBackend) places the
    // lock beside the complete filename, e.g. `models-store.json.lock`.
    // `with_extension("lock")` would instead produce `models-store.lock`,
    // allowing a Rust process and the upstream process to enter the same
    // store concurrently because they use different lock paths.
    let lock_path = file_lock_path(path);
    let _guard = FileLockGuard::acquire(&lock_path);
    f()
}

fn file_lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

struct FileLockGuard {
    path: PathBuf,
}

impl FileLockGuard {
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn acquire(path: &Path) -> Self {
        // Keep waiting for a live writer rather than overwriting its lock.
        // A lock older than the stale window is recoverable after a crashed
        // process, but an active writer must never be displaced.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return Self {
                        path: path.to_path_buf(),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .and_then(|modified| {
                            modified
                                .elapsed()
                                .map_err(|error| std::io::Error::other(error.to_string()))
                        })
                        .is_ok_and(|age| age >= Duration::from_secs(30));
                    if stale {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out acquiring model store lock {}",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    panic!(
                        "failed to acquire model store lock {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A metadata revision that detects same-millisecond replacements as well as
/// ordinary mtime changes. The upstream exposes only an mtime string, but the
/// inode/size components are needed here because atomic rename can preserve a
/// coarse filesystem timestamp.
pub fn file_revision(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let duration = mtime.duration_since(UNIX_EPOCH).ok()?;
    #[cfg(unix)]
    let inode_revision = meta
        .ino()
        .wrapping_mul(131)
        .wrapping_add(meta.dev().wrapping_mul(521));
    #[cfg(not(unix))]
    let inode_revision = 0;
    Some(
        (duration.as_nanos() as u64)
            .wrapping_add(meta.len().wrapping_mul(31))
            .wrapping_add(inode_revision),
    )
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
        Self {
            path,
            cache: Arc::new(Mutex::new(state)),
        }
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
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
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
        self.read_latest()
            .get(provider_id)
            .cloned()
            .map(ModelsStoreEntry::from)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn write_locked(&self, f: impl FnOnce(&mut StoredModels)) {
        // The lock is a sibling of the store file, so the parent must exist
        // before acquiring it. This matters for first-use catalogs (including
        // a clean agent directory) where no models-store directory exists yet.
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|error| {
                panic!(
                    "create models store directory {}: {error}",
                    parent.display()
                )
            });
        }
        with_file_lock(&self.path, || {
            let mut data = self.read_latest();
            f(&mut data);
            let serialized =
                serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".to_string());
            let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
            let name = self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("models-store.json");
            let temporary = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
            let write_result = (|| -> std::io::Result<()> {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&temporary)?;
                file.write_all(serialized.as_bytes())?;
                file.sync_all()?;
                fs::rename(&temporary, &self.path)?;
                // Persist the directory entry when the host filesystem
                // supports opening directories. Failure here does not undo a
                // valid atomic file replacement.
                let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
                Ok(())
            })();
            if let Err(error) = write_result {
                let _ = fs::remove_file(&temporary);
                panic!("write models-store.json {}: {error}", self.path.display());
            }
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        assert!(
            content.contains("\n    \"models\": ["),
            "content: {content}"
        );
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
    fn file_store_observes_external_replace_and_remove() {
        let path = tmp_path("external-revision");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = FileModelsStore::new(path.clone());
        store.write("demo", &entry_with_model());
        assert_eq!(store.read("demo").unwrap().models[0].id, "demo-1");

        let mut replacement = entry_with_model();
        replacement.models[0].id = "demo-external".to_string();
        let serialized = serde_json::to_string_pretty(&StoredModels::from([(
            "demo".to_string(),
            StoredModelsEntry::from(&replacement),
        )]))
        .unwrap();
        let temporary = path.with_extension("external.tmp");
        std::fs::write(&temporary, serialized).unwrap();
        std::fs::rename(&temporary, &path).unwrap();

        assert_eq!(
            store.read("demo").unwrap().models[0].id,
            "demo-external",
            "the existing store must invalidate its cached revision"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(store.read("demo").is_none());
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
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            "models-store.json"
        );
    }

    #[test]
    fn lock_path_keeps_the_complete_store_filename() {
        let path = PathBuf::from("/tmp/models-store.json");
        assert_eq!(
            file_lock_path(&path),
            PathBuf::from("/tmp/models-store.json.lock")
        );
    }
}
