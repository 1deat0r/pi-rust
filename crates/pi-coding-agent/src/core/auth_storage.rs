//! Credential storage backed by auth.json — port of
//! `packages/coding-agent/src/core/auth-storage.ts`.
//!
//! Provider auth orchestration belongs to ModelRuntime and pi-ai Models;
//! this module is the app-owned `CredentialStore` implementation (one
//! credential per provider, read-modify-write serialized through a sibling
//! `.lock` file).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::resolve_config_value::{is_command_config_value, resolve_config_value};
use crate::core::settings::strip_bom;

/// One type-tagged credential per provider — the shape of today's auth.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey {
        #[serde(skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        refresh: String,
        expires: u64,
        /// Opaque extension fields preserved verbatim (upstream keeps
        /// `[key: string]: unknown` on OAuthCredentials).
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

impl Credential {
    pub fn credential_type(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "api_key",
            Credential::OAuth { .. } => "oauth",
        }
    }
}

/// Non-secret credential metadata for account/status enumeration.
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialInfo {
    pub provider_id: String,
    pub credential_type: &'static str,
}

pub type AuthStorageData = BTreeMap<String, Credential>;

/// Optional cancellation for public auth and credential operations (upstream
/// AbortSignal — the port threads a shared atomic abort flag).
#[derive(Debug, Default, Clone)]
pub struct AuthOperationOptions {
    pub signal: Option<Arc<AtomicBool>>,
}

fn throw_if_aborted(signal: Option<&Arc<AtomicBool>>) -> Result<(), String> {
    if signal.is_some_and(|s| s.load(Ordering::SeqCst)) {
        return Err("Aborted".to_string());
    }
    Ok(())
}

/// Result of a locked mutation: `result` is returned to the caller; `next` is
/// the replacement file content (`None` = no write).
pub struct LockResult<T> {
    pub result: T,
    pub next: Option<String>,
}

/// Plain boxed future (output is the value itself).
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
/// Boxed future resolving to `LockResult<T>` — the `withLockAsync` callback
/// shape (upstream `(current) => Promise<LockResult<T>>`).
pub type LockFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = LockResult<T>> + Send + 'a>>;

/// `withLockAsync` callback: `(current) => Promise<LockResult<T>>`.
pub type LockCallback<'a, T> = Box<dyn FnMut(Option<&str>) -> BoxFuture<'a, Result<LockResult<T>, String>> + Send>;

/// Storage backend (upstream `AuthStorageBackend`): unlocked mutation of the
/// current value behind a lock. The two concrete backends (file, in-memory)
/// are represented directly; the upstream interface only ever instantiates
/// these two, so an enum keeps the dyn-compat machinery out.
pub enum AuthStorageBackend {
    File(FileAuthStorageBackend),
    InMemory(InMemoryAuthStorageBackend),
}

impl AuthStorageBackend {
    pub fn with_lock<T>(&self, f: &mut dyn FnMut(Option<&str>) -> LockResult<T>) -> T {
        match self {
            AuthStorageBackend::File(backend) => backend.with_lock_impl(f),
            AuthStorageBackend::InMemory(backend) => backend.with_lock_impl(f),
        }
    }

    pub fn with_lock_async<T>(
        &self,
        f: LockCallback<'_, T>,
        options: &AuthOperationOptions,
    ) -> BoxFuture<'_, Result<T, String>>
    where
        T: Send + 'static,
    {
        match self {
            AuthStorageBackend::File(backend) => backend.with_lock_async_impl(f, options),
            AuthStorageBackend::InMemory(backend) => backend.with_lock_async_impl(f, options),
        }
    }
}

fn rand_fraction() -> f64 {
    // Deterministic-enough jitter without pulling in a rand dep.
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    (nanos as f64) / 1_000_000_000.0
}

async fn sleep_abortable(duration: Duration, signal: Option<&Arc<AtomicBool>>) -> Result<(), String> {
    let deadline = std::time::Instant::now() + duration;
    loop {
        throw_if_aborted(signal)?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
    }
}

fn write_auth_file(path: &Path, content: &str) {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    if let Ok(mut file) = options.open(path) {
        use std::io::Write;
        let _ = file.write_all(content.as_bytes());
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

/// File-backed auth storage. Adds a `.<file>.lock` sibling (upstream
/// proper-lockfile): sync acquire retries 10×/20 ms like the settings
/// backend; async acquire backs off exponentially inside a 30 s stale window.
pub struct FileAuthStorageBackend {
    auth_path: PathBuf,
}

impl FileAuthStorageBackend {
    pub fn new(auth_path: PathBuf) -> Self {
        Self { auth_path }
    }

    fn ensure_parent_dir(&self) {
        if let Some(parent) = self.auth_path.parent() {
            let _ = fs::create_dir_all(parent);
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    fn ensure_file_exists(&self) {
        if !self.auth_path.exists() {
            write_auth_file(&self.auth_path, "{}");
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.auth_path.as_os_str().to_owned();
        path.push(".lock");
        PathBuf::from(path)
    }

    fn acquire_lock_sync_with_retry(&self) -> Result<fs::File, String> {
        let max_attempts = 10;
        let delay_ms = 20;
        let lock_path = self.lock_path();
        for attempt in 1..=max_attempts {
            match fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
                Ok(file) => return Ok(file),
                Err(error) => {
                    let is_elocked = error.kind() == std::io::ErrorKind::AlreadyExists;
                    if !is_elocked || attempt == max_attempts {
                        return Err(format!("Failed to acquire auth storage lock: {error}"));
                    }
                    std::thread::sleep(Duration::from_millis(delay_ms));
                }
            }
        }
        Err("Failed to acquire auth storage lock".to_string())
    }

    async fn acquire_lock_async(&self, signal: Option<&Arc<AtomicBool>>) -> Result<fs::File, String> {
        let stale_ms = 30_000;
        let max_delay_ms = 2_000;
        let deadline = std::time::Instant::now() + Duration::from_millis(stale_ms);
        let mut retry = 0u32;
        let lock_path = self.lock_path();
        loop {
            throw_if_aborted(signal)?;
            match fs::OpenOptions::new().write(true).create_new(true).open(&lock_path) {
                Ok(file) => {
                    if let Some(s) = signal {
                        if s.load(Ordering::SeqCst) {
                            let _ = fs::remove_file(&lock_path);
                            return Err("Aborted".to_string());
                        }
                    }
                    return Ok(file);
                }
                Err(error) => {
                    let is_elocked = error.kind() == std::io::ErrorKind::AlreadyExists;
                    throw_if_aborted(signal)?;
                    let remaining_ms = deadline.saturating_duration_since(std::time::Instant::now());
                    if !is_elocked || remaining_ms.is_zero() {
                        return Err(format!("Failed to acquire auth storage lock: {error}"));
                    }
                    let base_delay_ms =
                        std::cmp::min(10u64.saturating_mul(1u64 << retry.min(30)), max_delay_ms / 2);
                    retry += 1;
                    let delay_ms = std::cmp::min(
                        (base_delay_ms as f64 * (1.0 + rand_fraction())) as u64,
                        remaining_ms.as_millis() as u64,
                    );
                    sleep_abortable(Duration::from_millis(delay_ms), signal).await?;
                }
            }
        }
    }

    fn read_current(&self) -> Option<String> {
        fs::read_to_string(&self.auth_path).ok()
    }

    fn write_next(&self, next: String) {
        write_auth_file(&self.auth_path, &next);
    }
}

impl FileAuthStorageBackend {
    fn with_lock_impl<T>(&self, f: &mut dyn FnMut(Option<&str>) -> LockResult<T>) -> T {
        self.ensure_parent_dir();
        self.ensure_file_exists();

        let lock_file = match self.acquire_lock_sync_with_retry() {
            Ok(file) => file,
            Err(message) => panic!("{message}"),
        };
        let result = {
            let current = self.read_current();
            let LockResult { result, next } = f(current.as_deref());
            if let Some(next) = next {
                self.write_next(next);
            }
            result
        };
        drop(lock_file);
        let _ = fs::remove_file(self.lock_path());
        result
    }

    fn with_lock_async_impl<T>(
        &self,
        mut f: LockCallback<'_, T>,
        options: &AuthOperationOptions,
    ) -> BoxFuture<'_, Result<T, String>>
    where
        T: Send + 'static,
    {
        let this = self;
        let options = options.clone();
        Box::pin(async move {
            throw_if_aborted(options.signal.as_ref())?;
            this.ensure_parent_dir();
            this.ensure_file_exists();

            let lock_file = this.acquire_lock_async(options.signal.as_ref()).await?;
            let result = async {
                let current = this.read_current();
                let LockResult { result, next } = f(current.as_deref()).await?;
                if let Some(next) = next {
                    this.write_next(next);
                }
                Ok::<T, String>(result)
            }
            .await;
            drop(lock_file);
            let _ = fs::remove_file(this.lock_path());
            result
        })
    }
}

/// In-memory auth backend (upstream `InMemoryAuthStorageBackend`): a single
/// string value and a serialized async mutation chain.
#[derive(Default)]
pub struct InMemoryAuthStorageBackend {
    value: Arc<Mutex<Option<String>>>,
    async_chain: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl InMemoryAuthStorageBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InMemoryAuthStorageBackend {
    fn with_lock_impl<T>(&self, f: &mut dyn FnMut(Option<&str>) -> LockResult<T>) -> T {
        let current = self.value.lock().unwrap().clone();
        let LockResult { result, next } = f(current.as_deref());
        if let Some(next) = next {
            *self.value.lock().unwrap() = Some(next);
        }
        result
    }

    fn with_lock_async_impl<T>(
        &self,
        mut f: LockCallback<'_, T>,
        options: &AuthOperationOptions,
    ) -> BoxFuture<'_, Result<T, String>>
    where
        T: Send + 'static,
    {
        // Serialize operations through a chain so concurrent mutations run in
        // order (upstream queues on `this.asyncChain`).
        let value = self.value.clone();
        let value_for_task = value.clone();
        let previous = self.async_chain.lock().unwrap().take();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let current = value_for_task.lock().unwrap().clone();
            let locked = f(current.as_deref()).await;
            let _ = tx.send(locked);
        });
        *self.async_chain.lock().unwrap() = Some(handle);
        let options = options.clone();
        Box::pin(async move {
            throw_if_aborted(options.signal.as_ref())?;
            let locked = rx.await.map_err(|_| "Auth storage async chain failed".to_string())??;
            if let Some(next) = locked.next {
                *value.lock().unwrap() = Some(next);
            }
            Ok(locked.result)
        })
    }
}

/// Read-only credential store over an auth.json (upstream
/// `ReadOnlyAuthStorage`), with full load-time validation.
pub struct ReadOnlyAuthStorage {
    auth_path: PathBuf,
    data: Mutex<Option<AuthStorageData>>,
}

impl ReadOnlyAuthStorage {
    pub fn new(auth_path: PathBuf) -> Self {
        Self { auth_path, data: Mutex::new(None) }
    }

    fn load(&self) -> Result<AuthStorageData, String> {
        if let Some(data) = self.data.lock().unwrap().as_ref() {
            return Ok(data.clone());
        }
        let content = match fs::read_to_string(&self.auth_path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *self.data.lock().unwrap() = Some(AuthStorageData::new());
                return Ok(AuthStorageData::new());
            }
            Err(error) => return Err(format!("Failed to read auth.json: {error}")),
        };
        let parsed: Value = serde_json::from_str(strip_bom(&content))
            .map_err(|e| format!("Failed to read auth.json: {e}"))?;
        if !parsed.is_object() {
            return Err("Invalid auth.json: expected an object".to_string());
        }
        let mut data = AuthStorageData::new();
        for (provider_id, credential) in parsed.as_object().unwrap() {
            validate_credential(provider_id, credential)?;
            let credential: Credential = serde_json::from_value(credential.clone())
                .map_err(|e| format!("Invalid auth.json credential for provider \"{provider_id}\": {e}"))?;
            data.insert(provider_id.clone(), credential);
        }
        *self.data.lock().unwrap() = Some(data.clone());
        Ok(data)
    }

    pub async fn read(&self, provider_id: &str, options: &AuthOperationOptions) -> Result<Option<Credential>, String> {
        throw_if_aborted(options.signal.as_ref())?;
        let credential = self.load()?.get(provider_id).cloned();
        throw_if_aborted(options.signal.as_ref())?;
        let Some(credential) = credential else { return Ok(None) };
        // Command-configured keys are returned untouched; template keys are
        // resolved (upstream ReadOnlyAuthStorage.read).
        if let Credential::ApiKey { key: Some(key), .. } = &credential {
            if !is_command_config_value(key) {
                if let Some(resolved) = resolve_config_value(key, None) {
                    let mut resolved_credential = credential.clone();
                    if let Credential::ApiKey { key: k, .. } = &mut resolved_credential {
                        *k = Some(resolved);
                    }
                    return Ok(Some(resolved_credential));
                }
            }
        }
        Ok(Some(credential))
    }

    pub async fn list(&self, options: &AuthOperationOptions) -> Result<Vec<CredentialInfo>, String> {
        throw_if_aborted(options.signal.as_ref())?;
        let credentials = self
            .load()?
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                credential_type: credential.credential_type(),
            })
            .collect();
        throw_if_aborted(options.signal.as_ref())?;
        Ok(credentials)
    }

    pub async fn modify(
        &self,
        _provider_id: &str,
        _f: impl FnMut(Option<&Credential>) -> BoxFuture<'static, Result<Option<Credential>, String>> + Send,
        _options: &AuthOperationOptions,
    ) -> Result<Option<Credential>, String> {
        Err("Read-only credential storage cannot modify auth.json".to_string())
    }

    pub async fn delete(&self, _provider_id: &str, _options: &AuthOperationOptions) -> Result<(), String> {
        Err("Read-only credential storage cannot modify auth.json".to_string())
    }

    pub fn is_read_only_error() -> &'static str {
        "Read-only credential storage cannot modify auth.json"
    }
}

fn validate_credential(provider_id: &str, credential: &Value) -> Result<(), String> {
    if !credential.is_object() {
        return Err(format!("Invalid auth.json credential for provider \"{provider_id}\""));
    }
    let value = credential.as_object().unwrap();
    match value.get("type").and_then(Value::as_str) {
        Some("api_key") => {
            let valid_key = value.get("key").is_none_or(|k| k.is_string());
            let valid_env = match value.get("env") {
                None => true,
                Some(env) => env.as_object().is_some_and(|m| m.values().all(|entry| entry.is_string())),
            };
            if valid_key && valid_env {
                Ok(())
            } else {
                Err(format!("Invalid auth.json credential for provider \"{provider_id}\""))
            }
        }
        Some("oauth") => {
            let valid = value.get("access").is_some_and(Value::is_string)
                && value.get("refresh").is_some_and(Value::is_string)
                && value.get("expires").is_some_and(Value::is_number);
            if valid {
                Ok(())
            } else {
                Err(format!("Invalid auth.json credential for provider \"{provider_id}\""))
            }
        }
        _ => Err(format!("Invalid auth.json credential for provider \"{provider_id}\"")),
    }
}

/// `{dev}:{ino}:{size}:{mtimeNs}:{ctimeNs}` file revision (upstream
/// `getFileRevision`).
fn get_file_revision(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    Some(format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
        metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
    ))
}

/// Writable credential store backed by a JSON file (upstream `AuthStorage`).
pub struct AuthStorage {
    storage: AuthStorageBackend,
    auth_path: Option<PathBuf>,
    read_state: Mutex<AuthFileReadState>,
}

#[derive(Default, Clone)]
struct AuthFileReadState {
    data: AuthStorageData,
    revision: Option<String>,
}

impl AuthStorage {
    fn new(storage: AuthStorageBackend, auth_path: Option<PathBuf>) -> Self {
        let mut storage = Self { storage, auth_path, read_state: Mutex::new(AuthFileReadState::default()) };
        storage.reload();
        storage
    }

    pub fn create(auth_path: PathBuf) -> Self {
        Self::new(AuthStorageBackend::File(FileAuthStorageBackend::new(auth_path.clone())), Some(auth_path))
    }

    pub fn from_storage(storage: AuthStorageBackend) -> Self {
        Self::new(storage, None)
    }

    pub fn in_memory(data: AuthStorageData) -> Self {
        let mut storage = InMemoryAuthStorageBackend::new();
        // Initialize through the same value slot the backend treats as the
        // source of truth (upstream `AuthStorage.inMemory` writes the JSON).
        if !data.is_empty() {
            storage.value = Arc::new(Mutex::new(Some(serde_json::to_string_pretty(&data).unwrap())));
        }
        Self::from_storage(AuthStorageBackend::InMemory(storage))
    }

    fn parse_storage_data(content: Option<&str>) -> AuthStorageData {
        content
            .filter(|c| !c.trim().is_empty())
            .and_then(|c| serde_json::from_str::<AuthStorageData>(strip_bom(c)).ok())
            .unwrap_or_default()
    }

    fn read_under_lock(&self) -> Option<String> {
        let mut content: Option<String> = None;
        self.storage.with_lock(&mut |current| {
            content = current.map(|s| s.to_string());
            LockResult { result: (), next: None }
        });
        content
    }

    fn current_revision(&self) -> Option<String> {
        self.auth_path.as_deref().and_then(get_file_revision)
    }

    /// Reload credentials from storage (preserves the last valid snapshot on
    /// failure, like upstream).
    pub fn reload(&mut self) {
        let content = self.read_under_lock();
        let revision = self.current_revision();
        let data = Self::parse_storage_data(content.as_deref());
        let mut state = self.read_state.lock().unwrap();
        state.data = data;
        state.revision = revision;
    }

    async fn read_latest_data(&self, options: &AuthOperationOptions) -> Result<AuthStorageData, String> {
        throw_if_aborted(options.signal.as_ref())?;
        // In-memory stores are read freshly each time (their value is the
        // source of truth); file stores are cached by revision and reloaded
        // under the lock when the file changes.
        let (data, revision) = if self.auth_path.is_none() {
            let content = self.read_under_lock();
            (Self::parse_storage_data(content.as_deref()), None)
        } else {
            let cached = self.read_state.lock().unwrap();
            let revision = self.current_revision();
            if revision == cached.revision && cached.revision.is_some() {
                return Ok(cached.data.clone());
            }
            drop(cached);
            let content = self.read_under_lock();
            let revision = self.current_revision();
            let data = Self::parse_storage_data(content.as_deref());
            (data, revision)
        };
        {
            let mut state = self.read_state.lock().unwrap();
            state.data = data.clone();
            state.revision = revision;
        }
        throw_if_aborted(options.signal.as_ref())?;
        Ok(data)
    }

    pub async fn read(&self, provider: &str, options: &AuthOperationOptions) -> Result<Option<Credential>, String> {
        let credential = self.read_latest_data(options).await?.get(provider).cloned();
        throw_if_aborted(options.signal.as_ref())?;
        let Some(credential) = credential else { return Ok(None) };
        if let Credential::ApiKey { key: Some(key), .. } = &credential {
            if let Some(resolved) = resolve_config_value(key, None) {
                let mut resolved_credential = credential.clone();
                if let Credential::ApiKey { key: k, .. } = &mut resolved_credential {
                    *k = Some(resolved);
                }
                return Ok(Some(resolved_credential));
            }
        }
        Ok(Some(credential))
    }

    pub async fn modify(
        &self,
        provider: &str,
        mut f: impl FnMut(Option<&Credential>) -> BoxFuture<'static, Result<Option<Credential>, String>> + Send + 'static,
        options: &AuthOperationOptions,
    ) -> Result<Option<Credential>, String> {
        let provider = provider.to_string();
        let result = self
            .storage
            .with_lock_async(
                Box::new(move |content| {
                    let provider = provider.clone();
                    let current_data = Self::parse_storage_data(content);
                    let current = current_data.get(&provider).cloned();
                    let f_result = f(current.as_ref());
                    Box::pin(async move {
                        let next = f_result.await?;
                        if next.is_none() {
                            return Ok(LockResult {
                                result: current_data.get(&provider).cloned(),
                                next: None,
                            });
                        }
                        let mut merged = current_data.clone();
                        merged.insert(provider.clone(), next.unwrap());
                        Ok(LockResult {
                            result: merged.get(&provider).cloned(),
                            next: Some(serde_json::to_string_pretty(&merged).unwrap()),
                        })
                    })
                }),
                options,
            )
            .await?;
        // Refresh the in-memory snapshot from what was actually written.
        let content = self.read_under_lock();
        let revision = self.current_revision();
        let mut state = self.read_state.lock().unwrap();
        state.data = Self::parse_storage_data(content.as_deref());
        state.revision = revision;
        Ok(result)
    }

    pub async fn delete(&self, provider: &str, options: &AuthOperationOptions) -> Result<(), String> {
        let provider = provider.to_string();
        self.storage
            .with_lock_async(
                Box::new(move |content| {
                    let provider = provider.clone();
                    Box::pin(async move {
                        let mut current_data = Self::parse_storage_data(content);
                        current_data.remove(&provider);
                        Ok(LockResult {
                            result: (),
                            next: Some(serde_json::to_string_pretty(&current_data).unwrap()),
                        })
                    })
                }),
                options,
            )
            .await?;
        let content = self.read_under_lock();
        let revision = self.current_revision();
        let mut state = self.read_state.lock().unwrap();
        state.data = Self::parse_storage_data(content.as_deref());
        state.revision = revision;
        Ok(())
    }

    pub async fn list(&self, options: &AuthOperationOptions) -> Result<Vec<CredentialInfo>, String> {
        let entries = self.read_latest_data(options).await?;
        throw_if_aborted(options.signal.as_ref())?;
        Ok(entries
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                credential_type: credential.credential_type(),
            })
            .collect())
    }
}

/// One-off synchronous read of a stored credential from an auth.json file,
/// without instantiating a store or resolving configured key values.
pub fn read_stored_credential(provider_id: &str, auth_path: &Path) -> Option<Credential> {
    let content = fs::read_to_string(auth_path).ok()?;
    let data: AuthStorageData = serde_json::from_str(strip_bom(&content)).ok()?;
    data.get(provider_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_auth_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-auth-test-{}-{}", name, std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("auth.json")
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn in_memory_read_modify_delete_round_trip() {
        let storage = AuthStorage::in_memory(AuthStorageData::new());
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            let credential = Credential::ApiKey { key: Some("sk-test".into()), env: None };
            let saved = storage
                .modify("openai", move |current| {
                    assert!(current.is_none());
                    let credential = credential.clone();
                    Box::pin(async move { Ok(Some(credential)) })
                }, &opts)
                .await
                .unwrap();
            assert!(saved.is_some());

            let read = storage.read("openai", &opts).await.unwrap().unwrap();
            assert_eq!(read, Credential::ApiKey { key: Some("sk-test".into()), env: None });

            let list = storage.list(&opts).await.unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].provider_id, "openai");
            assert_eq!(list[0].credential_type, "api_key");

            storage.delete("openai", &opts).await.unwrap();
            assert!(storage.read("openai", &opts).await.unwrap().is_none());
        });
    }

    #[test]
    fn file_backend_persists_and_reads() {
        let path = temp_auth_path("file");
        let _ = fs::remove_file(&path);
        {
            let storage = AuthStorage::create(path.clone());
            let opts = AuthOperationOptions::default();
            runtime().block_on(async {
                storage
                    .modify("anthropic", |_| {
                        Box::pin(async move {
                            Ok(Some(Credential::ApiKey { key: Some("$PI_TEST_AUTH_FILE_KEY".into()), env: None }))
                        })
                    }, &opts)
                    .await
                    .unwrap();
            });
            drop(storage);
        }
        // Reads resolve env-template keys via resolveConfigValue semantics.
        std::env::set_var("PI_TEST_AUTH_FILE_KEY", "resolved-key");
        let fresh = AuthStorage::create(path.clone());
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            let read = fresh.read("anthropic", &opts).await.unwrap().unwrap();
            assert_eq!(read, Credential::ApiKey { key: Some("resolved-key".into()), env: None });
        });
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Path::new(&format!("{}.lock", path.display())));
    }

    #[test]
    fn read_only_missing_file_is_empty() {
        let store = ReadOnlyAuthStorage::new(temp_auth_path("missing"));
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            assert!(store.read("x", &opts).await.unwrap().is_none());
            assert_eq!(store.list(&opts).await.unwrap().len(), 0);
        });
    }

    #[test]
    fn read_only_rejects_invalid_credentials() {
        let path = temp_auth_path("ro");
        write_auth_file(&path, r#"{"openai":{"type":"api_key","key":"sk-1"},"bad":{"type":"unknown"}}"#);
        let store = ReadOnlyAuthStorage::new(path.clone());
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            assert!(store.read("openai", &opts).await.is_err());
        });
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_only_accepts_valid_credentials_and_resolves_templates() {
        let path = temp_auth_path("ro-ok");
        write_auth_file(&path, r#"{"openai":{"type":"api_key","key":"$PI_TEST_RO_KEY"}}"#);
        std::env::set_var("PI_TEST_RO_KEY", "resolved-ro");
        let store = ReadOnlyAuthStorage::new(path.clone());
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            let read = store.read("openai", &opts).await.unwrap().unwrap();
            assert_eq!(read, Credential::ApiKey { key: Some("resolved-ro".into()), env: None });
            let list = store.list(&opts).await.unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].credential_type, "api_key");
        });
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_only_modify_is_rejected() {
        let path = temp_auth_path("ro-reject");
        let _ = fs::remove_file(&path);
        write_auth_file(&path, "{}");
        let store = ReadOnlyAuthStorage::new(path);
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            assert!(store.modify("x", |_| {
                Box::pin(async move { Ok(None) })
            }, &opts).await.is_err());
            assert!(store.delete("x", &opts).await.is_err());
        });
    }

    #[test]
    fn read_stored_credential_is_resolution_free() {
        let path = temp_auth_path("stored");
        write_auth_file(&path, r#"{"google":{"type":"api_key","key":"!echo gg-key"}}"#);
        // Returns the raw stored value (command config untouched).
        let credential = read_stored_credential("google", &path).unwrap();
        assert_eq!(credential, Credential::ApiKey { key: Some("!echo gg-key".into()), env: None });
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn oauth_credential_round_trips_with_extra_fields() {
        let path = temp_auth_path("oauth");
        write_auth_file(
            &path,
            r#"{"github":{"type":"oauth","access":"acc","refresh":"ref","expires":123,"scope":["repo"]}}"#,
        );
        let credential = read_stored_credential("github", &path).unwrap();
        match credential {
            Credential::OAuth { access, refresh, expires, extra } => {
                assert_eq!(access, "acc");
                assert_eq!(refresh, "ref");
                assert_eq!(expires, 123);
                assert_eq!(extra.get("scope").and_then(Value::as_array).map(|a| a.len()), Some(1));
            }
            _ => panic!("expected oauth"),
        }
        let _ = fs::remove_file(&path);
    }
}
