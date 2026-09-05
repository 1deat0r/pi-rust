//! Credential storage backed by auth.json — port of
//! `packages/coding-agent/src/core/auth-storage.ts`.
//!
//! Provider auth orchestration belongs to ModelRuntime and pi-ai Models;
//! this module is the app-owned `CredentialStore` implementation (one
//! credential per provider, read-modify-write serialized through a sibling
//! `.lock` file).

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

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

/// Typed error for the async auth-storage surface. `Display` is the exact
/// message text the previous `Result<_, AuthStorageError>` implementation produced, so
/// banners and diagnostics are unchanged.
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct AuthStorageError(pub String);

impl From<String> for AuthStorageError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

fn throw_if_aborted(signal: Option<&Arc<AtomicBool>>) -> Result<(), AuthStorageError> {
    if signal.is_some_and(|s| s.load(Ordering::SeqCst)) {
        return Err("Aborted".to_string().into());
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
pub type LockFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = LockResult<T>> + Send + 'a>>;

/// `withLockAsync` callback: `(current) => Promise<LockResult<T>>`. The
/// callback parses the borrowed `current` into owned data before boxing, so
/// its returned future is `'static`.
pub type LockCallback<T> = Box<
    dyn FnOnce(Option<&str>) -> BoxFuture<'static, Result<LockResult<T>, AuthStorageError>> + Send,
>;

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
        f: LockCallback<T>,
        options: &AuthOperationOptions,
    ) -> BoxFuture<'_, Result<T, AuthStorageError>>
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as f64) / 1_000_000_000.0
}

async fn sleep_abortable(
    duration: Duration,
    signal: Option<&Arc<AtomicBool>>,
) -> Result<(), AuthStorageError> {
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

static AUTH_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn sync_parent_directory(parent: &Path) -> Result<(), AuthStorageError> {
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            AuthStorageError(format!(
                "failed to sync auth directory {}: {error}",
                parent.display()
            ))
        })
}

fn write_auth_file(path: &Path, content: &str) -> Result<(), AuthStorageError> {
    let Some(parent) = path.parent() else {
        return Err(AuthStorageError(format!(
            "auth path has no parent: {}",
            path.display()
        )));
    };
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create auth directory {}: {error}",
            parent.display()
        )
    })?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("auth.json");
    let counter = AUTH_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{name}.tmp-{}-{}-{counter}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&temp_path).map_err(|error| {
        format!(
            "failed to create temporary auth file {}: {error}",
            temp_path.display()
        )
    })?;
    let result = (|| {
        file.write_all(content.as_bytes())
            .map_err(|error| format!("failed to write auth file {}: {error}", path.display()))?;
        file.sync_all().map_err(|error| {
            format!(
                "failed to sync temporary auth file {}: {error}",
                temp_path.display()
            )
        })?;
        drop(file);
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!(
                "failed to set auth file permissions {}: {error}",
                temp_path.display()
            )
        })?;
        fs::rename(&temp_path, path)
            .map_err(|error| format!("failed to replace auth file {}: {error}", path.display()))?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

struct AuthLockGuard {
    _file: fs::File,
    path: PathBuf,
}

impl Drop for AuthLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn remove_dead_auth_lock(path: &Path) -> bool {
    let owner = fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok());
    let dead = match owner {
        Some(pid) if pid == std::process::id() => false,
        Some(pid) => !process_is_alive(pid),
        None => fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
            .map(|age| age >= Duration::from_secs(30))
            .unwrap_or(false),
    };
    dead && fs::remove_file(path).is_ok()
}

enum AuthLockOpenError {
    AlreadyExists,
    Other(String),
}

fn open_auth_lock(path: &Path) -> Result<fs::File, AuthLockOpenError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(AuthLockOpenError::AlreadyExists)
        }
        Err(error) => {
            return Err(AuthLockOpenError::Other(format!(
                "Failed to acquire auth storage lock: {error}"
            )))
        }
    };
    if let Err(error) = writeln!(file, "{}", std::process::id()).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(AuthLockOpenError::Other(format!(
            "Failed to initialize auth storage lock: {error}"
        )));
    }
    Ok(file)
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

    fn ensure_parent_dir(&self) -> Result<(), AuthStorageError> {
        if let Some(parent) = self.auth_path.parent() {
            let existed = parent.exists();
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create auth directory {}: {error}",
                    parent.display()
                )
            })?;
            if !existed {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        format!(
                            "failed to secure auth directory {}: {error}",
                            parent.display()
                        )
                    },
                )?;
            }
        }
        Ok(())
    }

    fn ensure_file_exists(&self) -> Result<(), AuthStorageError> {
        if !self.auth_path.exists() {
            write_auth_file(&self.auth_path, "{}")?;
        }
        Ok(())
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.auth_path.as_os_str().to_owned();
        path.push(".lock");
        PathBuf::from(path)
    }

    fn acquire_lock_sync_with_retry(&self) -> Result<AuthLockGuard, AuthStorageError> {
        let max_attempts = 10;
        let delay_ms = 20;
        let lock_path = self.lock_path();
        for attempt in 1..=max_attempts {
            match open_auth_lock(&lock_path) {
                Ok(file) => {
                    return Ok(AuthLockGuard {
                        _file: file,
                        path: lock_path,
                    })
                }
                Err(AuthLockOpenError::AlreadyExists) => {
                    if remove_dead_auth_lock(&lock_path) {
                        continue;
                    }
                    if attempt == max_attempts {
                        return Err("Failed to acquire auth storage lock: lock is already held"
                            .to_string()
                            .into());
                    }
                    std::thread::sleep(Duration::from_millis(delay_ms));
                }
                Err(AuthLockOpenError::Other(message)) => return Err(message.into()),
            }
        }
        Err("Failed to acquire auth storage lock".to_string().into())
    }

    async fn acquire_lock_async(
        &self,
        signal: Option<&Arc<AtomicBool>>,
    ) -> Result<AuthLockGuard, AuthStorageError> {
        let stale_ms = 30_000;
        let max_delay_ms = 2_000;
        let deadline = std::time::Instant::now() + Duration::from_millis(stale_ms);
        let mut retry = 0u32;
        let lock_path = self.lock_path();
        loop {
            throw_if_aborted(signal)?;
            match open_auth_lock(&lock_path) {
                Ok(file) => {
                    if let Some(s) = signal {
                        if s.load(Ordering::SeqCst) {
                            drop(file);
                            let _ = fs::remove_file(&lock_path);
                            return Err("Aborted".to_string().into());
                        }
                    }
                    return Ok(AuthLockGuard {
                        _file: file,
                        path: lock_path,
                    });
                }
                Err(AuthLockOpenError::AlreadyExists) => {
                    throw_if_aborted(signal)?;
                    let remaining_ms =
                        deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining_ms.is_zero() {
                        return Err("Failed to acquire auth storage lock: lock is already held"
                            .to_string()
                            .into());
                    }
                    if remove_dead_auth_lock(&lock_path) {
                        continue;
                    }
                    let base_delay_ms = std::cmp::min(
                        10u64.saturating_mul(1u64 << retry.min(30)),
                        max_delay_ms / 2,
                    );
                    retry += 1;
                    let delay_ms = std::cmp::min(
                        (base_delay_ms as f64 * (1.0 + rand_fraction())) as u64,
                        remaining_ms.as_millis() as u64,
                    );
                    sleep_abortable(Duration::from_millis(delay_ms), signal).await?;
                }
                Err(AuthLockOpenError::Other(message)) => return Err(message.into()),
            }
        }
    }

    fn read_current(&self) -> Result<Option<String>, AuthStorageError> {
        match fs::read_to_string(&self.auth_path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(AuthStorageError(format!(
                "failed to read auth file {}: {error}",
                self.auth_path.display()
            ))),
        }
    }

    fn write_next(&self, next: String) -> Result<(), AuthStorageError> {
        write_auth_file(&self.auth_path, &next)
    }

    fn read_consistent(&self) -> Result<Option<String>, AuthStorageError> {
        let lock_path = self.lock_path();
        if !self.auth_path.exists() && !lock_path.exists() {
            return Ok(None);
        }
        self.ensure_parent_dir()?;
        let _lock_guard = self.acquire_lock_sync_with_retry()?;
        self.read_current()
    }

    async fn read_consistent_async(
        &self,
        signal: Option<&Arc<AtomicBool>>,
    ) -> Result<Option<String>, AuthStorageError> {
        throw_if_aborted(signal)?;
        self.ensure_parent_dir()?;
        let _lock_guard = self.acquire_lock_async(signal).await?;
        self.ensure_file_exists()?;
        throw_if_aborted(signal)?;
        self.read_current()
    }
}

impl FileAuthStorageBackend {
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn with_lock_impl<T>(&self, f: &mut dyn FnMut(Option<&str>) -> LockResult<T>) -> T {
        if let Err(message) = self.ensure_parent_dir() {
            panic!("{message}");
        }
        let _lock_guard = match self.acquire_lock_sync_with_retry() {
            Ok(file) => file,
            Err(message) => panic!("{message}"),
        };
        if let Err(message) = self.ensure_file_exists() {
            panic!("{message}");
        }
        let result = {
            let current = self
                .read_current()
                .unwrap_or_else(|message| panic!("{message}"));
            let LockResult { result, next } = f(current.as_deref());
            if let Some(next) = next {
                self.write_next(next)
                    .unwrap_or_else(|message| panic!("{message}"));
            }
            result
        };
        result
    }

    fn with_lock_async_impl<T>(
        &self,
        f: LockCallback<T>,
        options: &AuthOperationOptions,
    ) -> BoxFuture<'_, Result<T, AuthStorageError>>
    where
        T: Send + 'static,
    {
        let this = self;
        let options = options.clone();
        Box::pin(async move {
            throw_if_aborted(options.signal.as_ref())?;
            this.ensure_parent_dir()?;

            let _lock_guard = this.acquire_lock_async(options.signal.as_ref()).await?;
            this.ensure_file_exists()?;
            let current = this.read_current()?;
            let LockResult { result, next } = f(current.as_deref()).await?;
            throw_if_aborted(options.signal.as_ref())?;
            if let Some(next) = next {
                this.write_next(next)?;
            }
            Ok::<T, AuthStorageError>(result)
        })
    }
}

/// In-memory auth backend (upstream `InMemoryAuthStorageBackend`): a single
/// string value and a serialized async mutation chain.
#[derive(Default)]
pub struct InMemoryAuthStorageBackend {
    value: Arc<Mutex<Option<String>>>,
    /// Serializes concurrent with_lock_async operations (upstream
    /// `asyncChain`) without requiring `'static` callbacks.
    async_lock: tokio::sync::Mutex<()>,
}

impl InMemoryAuthStorageBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InMemoryAuthStorageBackend {
    fn with_lock_impl<T>(&self, f: &mut dyn FnMut(Option<&str>) -> LockResult<T>) -> T {
        let current = self
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let LockResult { result, next } = f(current.as_deref());
        if let Some(next) = next {
            *self.value.lock().unwrap_or_else(|error| error.into_inner()) = Some(next);
        }
        result
    }

    fn with_lock_async_impl<T>(
        &self,
        f: LockCallback<T>,
        options: &AuthOperationOptions,
    ) -> BoxFuture<'_, Result<T, AuthStorageError>>
    where
        T: Send + 'static,
    {
        // Serialize concurrent mutations in order (upstream queues on
        // `this.asyncChain`); awaiting the tokio mutex gives the same
        // ordering without a spawned task.
        let value = self.value.clone();
        let options_clone = options.clone();
        Box::pin(async move {
            let _guard = self.async_lock.lock().await;
            throw_if_aborted(options_clone.signal.as_ref())?;
            let current = value
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let locked = f(current.as_deref()).await?;
            throw_if_aborted(options_clone.signal.as_ref())?;
            if let Some(next) = locked.next {
                *value.lock().unwrap_or_else(|error| error.into_inner()) = Some(next);
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
        Self {
            auth_path,
            data: Mutex::new(None),
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn load(&self) -> Result<AuthStorageData, AuthStorageError> {
        if let Some(data) = self
            .data
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            return Ok(data.clone());
        }
        let content = match fs::read_to_string(&self.auth_path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *self.data.lock().unwrap_or_else(|error| error.into_inner()) =
                    Some(AuthStorageData::new());
                return Ok(AuthStorageData::new());
            }
            Err(error) => {
                return Err(AuthStorageError(format!(
                    "Failed to read auth.json: {error}"
                )))
            }
        };
        let parsed: Value = serde_json::from_str(strip_bom(&content))
            .map_err(|e| format!("Failed to read auth.json: {e}"))?;
        if !parsed.is_object() {
            return Err("Invalid auth.json: expected an object".to_string().into());
        }
        let mut data = AuthStorageData::new();
        for (provider_id, credential) in parsed.as_object().unwrap() {
            validate_credential(provider_id, credential)?;
            let credential: Credential =
                serde_json::from_value(credential.clone()).map_err(|e| {
                    format!("Invalid auth.json credential for provider \"{provider_id}\": {e}")
                })?;
            data.insert(provider_id.clone(), credential);
        }
        *self.data.lock().unwrap_or_else(|error| error.into_inner()) = Some(data.clone());
        Ok(data)
    }

    pub async fn read(
        &self,
        provider_id: &str,
        options: &AuthOperationOptions,
    ) -> Result<Option<Credential>, AuthStorageError> {
        throw_if_aborted(options.signal.as_ref())?;
        let credential = self.load()?.get(provider_id).cloned();
        throw_if_aborted(options.signal.as_ref())?;
        let Some(credential) = credential else {
            return Ok(None);
        };
        // Command-configured keys are returned untouched; template and
        // literal keys are resolved with the credential's env map.
        Ok(Some(resolve_api_key_credential(&credential, true)))
    }

    pub async fn list(
        &self,
        options: &AuthOperationOptions,
    ) -> Result<Vec<CredentialInfo>, AuthStorageError> {
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
        _f: impl FnMut(
                Option<&Credential>,
            ) -> BoxFuture<'static, Result<Option<Credential>, AuthStorageError>>
            + Send,
        _options: &AuthOperationOptions,
    ) -> Result<Option<Credential>, AuthStorageError> {
        Err("Read-only credential storage cannot modify auth.json"
            .to_string()
            .into())
    }

    pub async fn delete(
        &self,
        _provider_id: &str,
        _options: &AuthOperationOptions,
    ) -> Result<(), AuthStorageError> {
        Err("Read-only credential storage cannot modify auth.json"
            .to_string()
            .into())
    }

    pub fn is_read_only_error() -> &'static str {
        "Read-only credential storage cannot modify auth.json"
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
fn validate_credential(provider_id: &str, credential: &Value) -> Result<(), AuthStorageError> {
    if !credential.is_object() {
        return Err(AuthStorageError(format!(
            "Invalid auth.json credential for provider \"{provider_id}\""
        )));
    }
    let value = credential.as_object().unwrap();
    match value.get("type").and_then(Value::as_str) {
        Some("api_key") => {
            let valid_key = value.get("key").is_none_or(|k| k.is_string());
            let valid_env = match value.get("env") {
                None => true,
                Some(env) => env
                    .as_object()
                    .is_some_and(|m| m.values().all(|entry| entry.is_string())),
            };
            if valid_key && valid_env {
                Ok(())
            } else {
                Err(AuthStorageError(format!(
                    "Invalid auth.json credential for provider \"{provider_id}\""
                )))
            }
        }
        Some("oauth") => {
            let valid = value.get("access").is_some_and(Value::is_string)
                && value.get("refresh").is_some_and(Value::is_string)
                && value.get("expires").is_some_and(Value::is_number);
            if valid {
                Ok(())
            } else {
                Err(AuthStorageError(format!(
                    "Invalid auth.json credential for provider \"{provider_id}\""
                )))
            }
        }
        _ => Err(AuthStorageError(format!(
            "Invalid auth.json credential for provider \"{provider_id}\""
        ))),
    }
}

/// Resolve the configured value of an API-key credential. The credential's
/// `env` map is provider-specific configuration and must take precedence over
/// the process environment, matching `resolveConfigValue(value, credential.env)`
/// in the upstream store.
fn resolve_api_key_credential(credential: &Credential, leave_commands: bool) -> Credential {
    let Credential::ApiKey {
        key: Some(key),
        env,
    } = credential
    else {
        return credential.clone();
    };
    if leave_commands && is_command_config_value(key) {
        return credential.clone();
    }

    let env_map = env.as_ref().map(|values| {
        values
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<std::collections::HashMap<_, _>>()
    });
    let resolved_key = resolve_config_value(key, env_map.as_ref());
    let mut resolved = credential.clone();
    if let Credential::ApiKey { key, .. } = &mut resolved {
        *key = resolved_key;
    }
    resolved
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

/// A file read snapshot is shared by every `AuthStorage` instance for the
/// same path. The upstream store does this so a model runtime, `/login`, and
/// `/logout` do not each maintain a stale copy of auth.json. Weak entries let
/// short-lived test/config paths disappear instead of retaining all paths for
/// the lifetime of the process.
fn shared_auth_file_read_state(path: &Path) -> Arc<Mutex<AuthFileReadState>> {
    static STATES: std::sync::OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<AuthFileReadState>>>>> =
        std::sync::OnceLock::new();
    let states = STATES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut states = states.lock().unwrap_or_else(|error| error.into_inner());
    states.retain(|_, state| state.strong_count() > 0);
    if let Some(state) = states.get(path).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(Mutex::new(AuthFileReadState::default()));
    states.insert(path.to_path_buf(), Arc::downgrade(&state));
    state
}

type AuthFileReloadResult = Result<(AuthStorageData, Option<String>), AuthStorageError>;

/// One in-flight file reload shared by all readers of the same path. The
/// future itself is run in a task so an aborting caller can stop waiting while
/// another reader continues to use the same reload result.
struct AuthFileReload {
    result: Mutex<Option<AuthFileReloadResult>>,
    notify: tokio::sync::Notify,
    readers: AtomicUsize,
    cancel: Arc<AtomicBool>,
}

fn spawn_auth_file_reload(path: PathBuf, reload: Arc<AuthFileReload>) {
    tokio::spawn(async move {
        let backend = FileAuthStorageBackend::new(path.clone());
        let result = async {
            let content = backend.read_consistent_async(Some(&reload.cancel)).await?;
            let data = AuthStorage::parse_storage_data(content.as_deref())?;
            Ok((data, get_file_revision(&path)))
        }
        .await;
        *reload
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
        reload.notify.notify_waiters();
    });
}

async fn await_auth_file_reload(
    reload: &AuthFileReload,
    signal: Option<&Arc<AtomicBool>>,
) -> AuthFileReloadResult {
    loop {
        throw_if_aborted(signal)?;
        let notified = reload.notify.notified();
        if let Some(result) = reload
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            return result;
        }
        if let Some(signal) = signal {
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    throw_if_aborted(Some(signal))?;
                }
            }
        } else {
            notified.await;
        }
    }
}

/// Writable credential store backed by a JSON file (upstream `AuthStorage`).
pub struct AuthStorage {
    storage: AuthStorageBackend,
    auth_path: Option<PathBuf>,
    read_state: Arc<Mutex<AuthFileReadState>>,
}

#[derive(Default, Clone)]
struct AuthFileReadState {
    data: AuthStorageData,
    revision: Option<String>,
    reload: Option<Arc<AuthFileReload>>,
}

impl AuthStorage {
    fn new(storage: AuthStorageBackend, auth_path: Option<PathBuf>) -> Self {
        let read_state = auth_path
            .as_deref()
            .map(shared_auth_file_read_state)
            .unwrap_or_default();
        let mut storage = Self {
            storage,
            auth_path,
            read_state,
        };
        let should_reload = storage
            .auth_path
            .as_deref()
            .map(|path| {
                let revision = get_file_revision(path);
                let cached_revision = storage
                    .read_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .revision
                    .clone();
                revision.is_none() || revision != cached_revision
            })
            .unwrap_or(true);
        if should_reload {
            storage.reload();
        }
        storage
    }

    pub fn create(auth_path: PathBuf) -> Self {
        Self::new(
            AuthStorageBackend::File(FileAuthStorageBackend::new(auth_path.clone())),
            Some(auth_path),
        )
    }

    pub fn from_storage(storage: AuthStorageBackend) -> Self {
        Self::new(storage, None)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn in_memory(data: AuthStorageData) -> Self {
        let mut storage = InMemoryAuthStorageBackend::new();
        // Initialize through the same value slot the backend treats as the
        // source of truth (upstream `AuthStorage.inMemory` writes the JSON).
        if !data.is_empty() {
            storage.value = Arc::new(Mutex::new(Some(
                serde_json::to_string_pretty(&data).unwrap(),
            )));
        }
        Self::from_storage(AuthStorageBackend::InMemory(storage))
    }

    fn parse_storage_data(content: Option<&str>) -> Result<AuthStorageData, AuthStorageError> {
        let Some(content) = content.filter(|c| !c.trim().is_empty()) else {
            return Ok(AuthStorageData::new());
        };
        serde_json::from_str::<AuthStorageData>(strip_bom(content))
            .map_err(|error| AuthStorageError(format!("Failed to read auth.json: {error}")))
    }

    fn read_under_lock(&self) -> Option<String> {
        let mut content: Option<String> = None;
        self.storage.with_lock(&mut |current| {
            content = current.map(|s| s.to_string());
            LockResult {
                result: (),
                next: None,
            }
        });
        content
    }

    fn current_revision(&self) -> Option<String> {
        self.auth_path.as_deref().and_then(get_file_revision)
    }

    async fn wait_for_file_reload(
        &self,
        reload: Arc<AuthFileReload>,
        options: &AuthOperationOptions,
    ) -> Result<AuthStorageData, AuthStorageError> {
        let result = await_auth_file_reload(&reload, options.signal.as_ref()).await;
        let remove_reload = {
            let mut state = self
                .read_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            reload.readers.fetch_sub(1, Ordering::SeqCst);
            if state
                .reload
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &reload))
                && reload.readers.load(Ordering::SeqCst) == 0
            {
                state.reload = None;
                true
            } else {
                false
            }
        };
        if remove_reload {
            reload.cancel.store(true, Ordering::SeqCst);
        }

        match result {
            Ok((data, revision)) => {
                let mut state = self
                    .read_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.data = data.clone();
                state.revision = revision;
                drop(state);
                throw_if_aborted(options.signal.as_ref())?;
                Ok(data)
            }
            Err(_error) if options.signal.is_none() => Ok(self
                .read_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .data
                .clone()),
            Err(error) => Err(error),
        }
    }

    /// Reload credentials from storage (preserves the last valid snapshot on
    /// failure, like upstream).
    pub fn reload(&mut self) {
        let content = self.read_under_lock();
        let revision = self.current_revision();
        if let Ok(data) = Self::parse_storage_data(content.as_deref()) {
            let mut state = self
                .read_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.data = data;
            state.revision = revision;
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    async fn read_latest_data(
        &self,
        options: &AuthOperationOptions,
    ) -> Result<AuthStorageData, AuthStorageError> {
        throw_if_aborted(options.signal.as_ref())?;
        // In-memory stores are read freshly each time (their value is the
        // source of truth); file stores are cached by revision and reloaded
        // under the lock when the file changes.
        let (data, revision) = if self.auth_path.is_none() {
            let content = self.read_under_lock();
            let data = match Self::parse_storage_data(content.as_deref()) {
                Ok(data) => data,
                Err(error) if options.signal.is_some() => return Err(error),
                Err(_) => {
                    return Ok(self
                        .read_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .data
                        .clone())
                }
            };
            (data, None)
        } else {
            {
                let cached = self
                    .read_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let revision = self.current_revision();
                if revision == cached.revision && cached.revision.is_some() {
                    return Ok(cached.data.clone());
                }
            }
            let (reload, should_start) = {
                let mut state = self
                    .read_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if let Some(existing) = state.reload.clone() {
                    existing.readers.fetch_add(1, Ordering::SeqCst);
                    (existing, false)
                } else {
                    let reload = Arc::new(AuthFileReload {
                        result: Mutex::new(None),
                        notify: tokio::sync::Notify::new(),
                        readers: AtomicUsize::new(1),
                        cancel: Arc::new(AtomicBool::new(false)),
                    });
                    state.reload = Some(reload.clone());
                    (reload, true)
                }
            };
            if should_start {
                spawn_auth_file_reload(self.auth_path.clone().unwrap(), reload.clone());
            }
            return self.wait_for_file_reload(reload, options).await;
        };
        {
            let mut state = self
                .read_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.data = data.clone();
            state.revision = revision;
        }
        throw_if_aborted(options.signal.as_ref())?;
        Ok(data)
    }

    pub async fn read(
        &self,
        provider: &str,
        options: &AuthOperationOptions,
    ) -> Result<Option<Credential>, AuthStorageError> {
        let credential = self.read_latest_data(options).await?.get(provider).cloned();
        throw_if_aborted(options.signal.as_ref())?;
        let Some(credential) = credential else {
            return Ok(None);
        };
        Ok(Some(resolve_api_key_credential(&credential, false)))
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub async fn modify(
        &self,
        provider: &str,
        mut f: impl FnMut(
                Option<&Credential>,
            ) -> BoxFuture<'static, Result<Option<Credential>, AuthStorageError>>
            + Send
            + 'static,
        options: &AuthOperationOptions,
    ) -> Result<Option<Credential>, AuthStorageError> {
        let provider = provider.to_string();
        let result = self
            .storage
            .with_lock_async(
                Box::new(move |content| {
                    let provider = provider.clone();
                    let current_data = match Self::parse_storage_data(content) {
                        Ok(data) => data,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
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
        let mut state = self
            .read_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.data = Self::parse_storage_data(content.as_deref())?;
        state.revision = revision;
        Ok(result)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub async fn delete(
        &self,
        provider: &str,
        options: &AuthOperationOptions,
    ) -> Result<(), AuthStorageError> {
        let provider = provider.to_string();
        self.storage
            .with_lock_async(
                Box::new(move |content| {
                    let provider = provider.clone();
                    let mut current_data = match Self::parse_storage_data(content) {
                        Ok(data) => data,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
                    current_data.remove(&provider);
                    let next = serde_json::to_string_pretty(&current_data).unwrap();
                    Box::pin(async move {
                        Ok(LockResult {
                            result: (),
                            next: Some(next),
                        })
                    })
                }),
                options,
            )
            .await?;
        let content = self.read_under_lock();
        let revision = self.current_revision();
        let mut state = self
            .read_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.data = Self::parse_storage_data(content.as_deref())?;
        state.revision = revision;
        Ok(())
    }

    pub async fn list(
        &self,
        options: &AuthOperationOptions,
    ) -> Result<Vec<CredentialInfo>, AuthStorageError> {
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
    let content = FileAuthStorageBackend::new(auth_path.to_path_buf())
        .read_consistent()
        .ok()
        .flatten()?;
    let data: AuthStorageData = serde_json::from_str(strip_bom(&content)).ok()?;
    data.get(provider_id).cloned()
}

/// Synchronous `pi-ai` credential-store adapter used by the model runtime.
///
/// The interactive login command writes through [`AuthStorage`], while model
/// requests resolve through `pi-ai::Models`, whose trait is synchronous in the
/// Rust port. Reading the file on every operation keeps both surfaces pointed
/// at the same source of truth: a credential saved by `/login` is immediately
/// usable by the already-running TUI, and `/logout` takes effect without a
/// restart.
#[derive(Clone)]
pub struct FileCredentialStore {
    auth_path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(auth_path: PathBuf) -> Self {
        Self { auth_path }
    }

    fn read_all(&self) -> AuthStorageData {
        FileAuthStorageBackend::new(self.auth_path.clone())
            .read_consistent()
            .ok()
            .flatten()
            .and_then(|content| serde_json::from_str::<AuthStorageData>(strip_bom(&content)).ok())
            .unwrap_or_default()
    }
}

fn pi_ai_credential_from_storage(credential: &Credential) -> pi_ai::auth::Credential {
    match credential {
        Credential::ApiKey { key, env } => {
            pi_ai::auth::Credential::ApiKey(pi_ai::auth::ApiKeyCredential {
                key: key.clone(),
                env: env.clone(),
            })
        }
        Credential::OAuth {
            access,
            refresh,
            expires,
            extra,
        } => pi_ai::auth::Credential::OAuth(pi_ai::auth::OAuthCredential {
            access: access.clone(),
            refresh: refresh.clone(),
            expires: *expires,
            extra: extra.clone(),
        }),
    }
}

fn storage_credential_from_pi_ai(credential: &pi_ai::auth::Credential) -> Credential {
    match credential {
        pi_ai::auth::Credential::ApiKey(credential) => Credential::ApiKey {
            key: credential.key.clone(),
            env: credential.env.clone(),
        },
        pi_ai::auth::Credential::OAuth(credential) => Credential::OAuth {
            access: credential.access.clone(),
            refresh: credential.refresh.clone(),
            expires: credential.expires,
            extra: credential.extra.clone(),
        },
    }
}

impl pi_ai::auth::CredentialStore for FileCredentialStore {
    fn read(&self, provider_id: &str) -> Option<pi_ai::auth::Credential> {
        self.read_all().get(provider_id).map(|credential| {
            let resolved = resolve_api_key_credential(credential, false);
            pi_ai_credential_from_storage(&resolved)
        })
    }

    fn list(&self) -> Vec<pi_ai::auth::CredentialInfo> {
        self.read_all()
            .into_iter()
            .map(|(provider_id, credential)| pi_ai::auth::CredentialInfo {
                provider_id,
                credential_type: credential.credential_type(),
            })
            .collect()
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn modify(
        &self,
        provider_id: &str,
        f: &dyn Fn(Option<&pi_ai::auth::Credential>) -> Option<pi_ai::auth::Credential>,
    ) -> Option<pi_ai::auth::Credential> {
        let backend = FileAuthStorageBackend::new(self.auth_path.clone());
        let mut result = None;
        backend.with_lock_impl(&mut |content| {
            let mut data = AuthStorage::parse_storage_data(content)
                .unwrap_or_else(|error| panic!("Cannot modify malformed auth.json: {error}"));
            let current = data.get(provider_id).map(pi_ai_credential_from_storage);
            let next = f(current.as_ref());
            let next_content = next.map(|next| {
                data.insert(
                    provider_id.to_string(),
                    storage_credential_from_pi_ai(&next),
                );
                result = data.get(provider_id).map(pi_ai_credential_from_storage);
                serde_json::to_string_pretty(&data).unwrap()
            });
            if next_content.is_none() {
                result = current;
            }
            LockResult {
                result: (),
                next: next_content,
            }
        });
        result
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn delete(&self, provider_id: &str) {
        let backend = FileAuthStorageBackend::new(self.auth_path.clone());
        backend.with_lock_impl(&mut |content| {
            let mut data = AuthStorage::parse_storage_data(content)
                .unwrap_or_else(|error| panic!("Cannot delete from malformed auth.json: {error}"));
            let next = data
                .remove(provider_id)
                .map(|_| serde_json::to_string_pretty(&data).unwrap());
            LockResult { result: (), next }
        });
    }
}

/// The default OAuth validity window used by upstream auth resolution.
pub const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: u64 = 5 * 60 * 1000;

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn oauth_credential_for_pi_ai(credential: &Credential) -> Option<pi_ai::auth::OAuthCredential> {
    let Credential::OAuth {
        access,
        refresh,
        expires,
        extra,
    } = credential
    else {
        return None;
    };
    Some(pi_ai::auth::OAuthCredential {
        access: access.clone(),
        refresh: refresh.clone(),
        expires: *expires,
        extra: extra.clone(),
    })
}

fn credential_from_pi_ai(credential: pi_ai::auth::OAuthCredential) -> Credential {
    Credential::OAuth {
        access: credential.access,
        refresh: credential.refresh,
        expires: credential.expires,
        extra: credential.extra,
    }
}

/// Refresh an expired OAuth credential under AuthStorage's serialized modify
/// lock and persist the provider-returned credential (including rotated
/// refresh tokens and opaque extension fields).
pub async fn refresh_oauth_credential_in_storage(
    storage: &AuthStorage,
    provider_id: &str,
    oauth: Arc<dyn pi_ai::auth::OAuthAuth>,
    min_validity_ms: Option<u64>,
    signal: Option<Arc<AtomicBool>>,
) -> Result<Option<Credential>, AuthStorageError> {
    let options = AuthOperationOptions {
        signal: signal.clone(),
    };
    let Some(stored) = storage.read(provider_id, &options).await? else {
        return Ok(None);
    };
    if !matches!(stored, Credential::OAuth { .. }) {
        return Ok(Some(stored));
    }

    let minimum_validity_ms = DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_validity_ms.unwrap_or(0));
    let now = current_time_ms();
    let stored_expires = match &stored {
        Credential::OAuth { expires, .. } => *expires,
        Credential::ApiKey { .. } => unreachable!("API keys return before expiry checks"),
    };
    if now.saturating_add(minimum_validity_ms) < stored_expires {
        return Ok(Some(stored));
    }

    let refresh_signal = signal.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let oauth_for_lock = oauth.clone();
    let provider_name = provider_id.to_string();
    let minimum_validity_for_lock = minimum_validity_ms;
    let modified = storage
        .modify(
            provider_id,
            move |current| {
                let current = current.cloned();
                let oauth = oauth_for_lock.clone();
                let refresh_signal = refresh_signal.clone();
                let provider_name = provider_name.clone();
                Box::pin(async move {
                    let Some(current) = current else {
                        return Ok(None);
                    };
                    let Some(stored_oauth) = oauth_credential_for_pi_ai(&current) else {
                        return Ok(None);
                    };
                    if current_time_ms().saturating_add(minimum_validity_for_lock)
                        < stored_oauth.expires
                    {
                        return Ok(None);
                    }
                    let refreshed = oauth
                        .refresh(&stored_oauth, &refresh_signal)
                        .await
                        .map_err(|error| {
                            format!("OAuth refresh failed for {provider_name}: {error}")
                        })?;
                    Ok(Some(credential_from_pi_ai(refreshed)))
                })
            },
            &options,
        )
        .await?;

    if let Some(min_validity_ms) = min_validity_ms {
        if let Some(Credential::OAuth { expires, .. }) = modified.as_ref() {
            if current_time_ms().saturating_add(min_validity_ms) >= *expires {
                return Err(AuthStorageError(format!(
                    "OAuth refresh returned a token that expires too soon for {provider_id}"
                )));
            }
        }
    }
    Ok(modified)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_auth_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pi-auth-test-{}-{}", name, std::process::id()));
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
            let credential = Credential::ApiKey {
                key: Some("sk-test".into()),
                env: None,
            };
            let saved = storage
                .modify(
                    "openai",
                    move |current| {
                        assert!(current.is_none());
                        let credential = credential.clone();
                        Box::pin(async move { Ok(Some(credential)) })
                    },
                    &opts,
                )
                .await
                .unwrap();
            assert!(saved.is_some());

            let read = storage.read("openai", &opts).await.unwrap().unwrap();
            assert_eq!(
                read,
                Credential::ApiKey {
                    key: Some("sk-test".into()),
                    env: None
                }
            );

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
                    .modify(
                        "anthropic",
                        |_| {
                            Box::pin(async move {
                                Ok(Some(Credential::ApiKey {
                                    key: Some("$PI_TEST_AUTH_FILE_KEY".into()),
                                    env: None,
                                }))
                            })
                        },
                        &opts,
                    )
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
            assert_eq!(
                read,
                Credential::ApiKey {
                    key: Some("resolved-key".into()),
                    env: None
                }
            );
        });
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Path::new(&format!("{}.lock", path.display())));
    }

    #[test]
    fn file_credential_store_is_live_model_runtime_source() {
        let path = temp_auth_path("runtime-source");
        let _ = fs::remove_file(&path);
        let credentials = Arc::new(FileCredentialStore::new(path.clone()));
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions {
            credentials: Some(credentials),
            ..Default::default()
        });
        models.set_provider(pi_ai::providers::openai_codex_provider());

        assert!(models.get_auth("openai-codex", None).is_none());
        write_auth_file(
            &path,
            &serde_json::to_string_pretty(&AuthStorageData::from([(
                "openai-codex".to_string(),
                Credential::OAuth {
                    access: "fixture-access".to_string(),
                    refresh: "fixture-refresh".to_string(),
                    expires: current_time_ms() + 60 * 60 * 1000,
                    extra: BTreeMap::new(),
                },
            )]))
            .unwrap(),
        )
        .unwrap();
        let auth = models
            .get_auth("openai-codex", None)
            .expect("externally added OAuth is visible to the live Models facade");
        assert_eq!(auth.auth.api_key.as_deref(), Some("fixture-access"));

        write_auth_file(
            &path,
            &serde_json::to_string_pretty(&AuthStorageData::from([(
                "openai-codex".to_string(),
                Credential::OAuth {
                    access: "replacement-access".to_string(),
                    refresh: "replacement-refresh".to_string(),
                    expires: current_time_ms() + 60 * 60 * 1000,
                    extra: BTreeMap::new(),
                },
            )]))
            .unwrap(),
        )
        .unwrap();
        let auth = models
            .get_auth("openai-codex", None)
            .expect("externally replaced OAuth is visible to the live Models facade");
        assert_eq!(auth.auth.api_key.as_deref(), Some("replacement-access"));

        fs::remove_file(&path).unwrap();
        assert!(models.get_auth("openai-codex", None).is_none());
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
        write_auth_file(
            &path,
            r#"{"openai":{"type":"api_key","key":"sk-1"},"bad":{"type":"unknown"}}"#,
        )
        .unwrap();
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
        write_auth_file(
            &path,
            r#"{"openai":{"type":"api_key","key":"$PI_TEST_RO_KEY"}}"#,
        )
        .unwrap();
        std::env::set_var("PI_TEST_RO_KEY", "resolved-ro");
        let store = ReadOnlyAuthStorage::new(path.clone());
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            let read = store.read("openai", &opts).await.unwrap().unwrap();
            assert_eq!(
                read,
                Credential::ApiKey {
                    key: Some("resolved-ro".into()),
                    env: None
                }
            );
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
        write_auth_file(&path, "{}").unwrap();
        let store = ReadOnlyAuthStorage::new(path);
        let opts = AuthOperationOptions::default();
        runtime().block_on(async {
            assert!(store
                .modify("x", |_| { Box::pin(async move { Ok(None) }) }, &opts)
                .await
                .is_err());
            assert!(store.delete("x", &opts).await.is_err());
        });
    }

    #[test]
    fn read_stored_credential_is_resolution_free() {
        let path = temp_auth_path("stored");
        write_auth_file(
            &path,
            r#"{"google":{"type":"api_key","key":"!echo gg-key"}}"#,
        )
        .unwrap();
        // Returns the raw stored value (command config untouched).
        let credential = read_stored_credential("google", &path).unwrap();
        assert_eq!(
            credential,
            Credential::ApiKey {
                key: Some("!echo gg-key".into()),
                env: None
            }
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn oauth_credential_round_trips_with_extra_fields() {
        let path = temp_auth_path("oauth");
        write_auth_file(
            &path,
            r#"{"github":{"type":"oauth","access":"acc","refresh":"ref","expires":123,"scope":["repo"]}}"#,
        )
        .unwrap();
        let credential = read_stored_credential("github", &path).unwrap();
        match credential {
            Credential::OAuth {
                access,
                refresh,
                expires,
                extra,
            } => {
                assert_eq!(access, "acc");
                assert_eq!(refresh, "ref");
                assert_eq!(expires, 123);
                assert_eq!(
                    extra
                        .get("scope")
                        .and_then(Value::as_array)
                        .map(|a| a.len()),
                    Some(1)
                );
            }
            _ => panic!("expected oauth"),
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn credential_env_map_wins_for_read_and_model_store_resolution() {
        let env_name = format!("PI_RUST_AUTH_SCOPED_{}", std::process::id());
        let scoped_value = "provider-scoped-value";
        let credential = Credential::ApiKey {
            key: Some(format!("${env_name}")),
            env: Some(BTreeMap::from([(
                env_name.clone(),
                scoped_value.to_string(),
            )])),
        };

        let mut data = AuthStorageData::new();
        data.insert("scoped".to_string(), credential.clone());
        let storage = AuthStorage::in_memory(data);
        let options = AuthOperationOptions::default();
        runtime().block_on(async {
            assert_eq!(
                storage.read("scoped", &options).await.unwrap(),
                Some(Credential::ApiKey {
                    key: Some(scoped_value.to_string()),
                    env: Some(credential_env(env_name.clone())),
                })
            );
        });

        let path = temp_auth_path("scoped-env");
        write_auth_file(
            &path,
            &serde_json::to_string(&AuthStorageData::from([("scoped".to_string(), credential)]))
                .unwrap(),
        )
        .unwrap();
        let read_only = ReadOnlyAuthStorage::new(path.clone());
        runtime().block_on(async {
            assert_eq!(
                read_only.read("scoped", &options).await.unwrap(),
                Some(Credential::ApiKey {
                    key: Some(scoped_value.to_string()),
                    env: Some(credential_env(env_name.clone())),
                })
            );
        });

        use pi_ai::auth::CredentialStore as _;
        let store = FileCredentialStore::new(path.clone());
        let resolved = store.read("scoped").expect("stored credential");
        match resolved {
            pi_ai::auth::Credential::ApiKey(api_key) => {
                assert_eq!(api_key.key.as_deref(), Some(scoped_value));
                assert_eq!(api_key.env, Some(credential_env(env_name)));
            }
            _ => panic!("expected api-key credential"),
        }
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Path::new(&format!("{}.lock", path.display())));
    }

    #[test]
    fn unresolved_template_key_is_not_exposed_as_a_literal() {
        let env_name = format!("PI_RUST_AUTH_MISSING_{}", std::process::id());
        let credential = Credential::ApiKey {
            key: Some(format!("${env_name}")),
            env: None,
        };
        let mut data = AuthStorageData::new();
        data.insert("missing".to_string(), credential);
        let storage = AuthStorage::in_memory(data);
        let options = AuthOperationOptions::default();
        let resolved = runtime()
            .block_on(storage.read("missing", &options))
            .unwrap()
            .unwrap();
        assert_eq!(
            resolved,
            Credential::ApiKey {
                key: None,
                env: None
            }
        );
    }

    #[test]
    fn invalid_file_reload_preserves_last_valid_snapshot() {
        let path = temp_auth_path("reload-invalid");
        write_auth_file(&path, r#"{"provider":{"type":"api_key","key":"stable"}}"#).unwrap();
        let mut storage = AuthStorage::create(path.clone());
        let options = AuthOperationOptions::default();
        let before = runtime()
            .block_on(storage.read("provider", &options))
            .unwrap();

        fs::write(&path, "{not valid json").unwrap();
        storage.reload();
        let after = runtime()
            .block_on(storage.read("provider", &options))
            .unwrap();
        assert_eq!(after, before);

        let signaled = AuthOperationOptions {
            signal: Some(Arc::new(AtomicBool::new(false))),
        };
        assert!(runtime()
            .block_on(storage.read("provider", &signaled))
            .is_err());
        assert!(runtime()
            .block_on(storage.modify("other", |_| Box::pin(async { Ok(None) }), &options,))
            .is_err());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Path::new(&format!("{}.lock", path.display())));
    }

    #[test]
    fn file_reload_state_is_shared_and_coalesces_waiting_readers() {
        let path = temp_auth_path("reload-coalesced");
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&lock_path);
        write_auth_file(&path, r#"{"provider":{"type":"api_key","key":"old"}}"#).unwrap();

        let first = Arc::new(AuthStorage::create(path.clone()));
        let second = Arc::new(AuthStorage::create(path.clone()));
        assert!(Arc::ptr_eq(&first.read_state, &second.read_state));

        // Change the revision and hold the lock so the first reload remains
        // in flight while the second reader joins it.
        write_auth_file(
            &path,
            r#"{"provider":{"type":"api_key","key":"new-value"}}"#,
        )
        .unwrap();
        fs::write(&lock_path, format!("{}\n", std::process::id())).unwrap();

        let result = runtime().block_on(async {
            let first_task = tokio::spawn({
                let first = first.clone();
                async move {
                    first
                        .read("provider", &AuthOperationOptions::default())
                        .await
                }
            });
            let first_started = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let readers = first
                        .read_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .reload
                        .as_ref()
                        .map(|reload| reload.readers.load(Ordering::SeqCst))
                        .unwrap_or(0);
                    if readers >= 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok();
            if !first_started {
                let _ = first_task.await;
                return Err("first auth reader did not start".to_string());
            }

            let second_task = tokio::spawn({
                let second = second.clone();
                async move {
                    second
                        .read("provider", &AuthOperationOptions::default())
                        .await
                }
            });
            let two_readers = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let readers = first
                        .read_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .reload
                        .as_ref()
                        .map(|reload| reload.readers.load(Ordering::SeqCst))
                        .unwrap_or(0);
                    if readers >= 2 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok();
            let reload_count = first
                .read_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .reload
                .as_ref()
                .map(|reload| reload.readers.load(Ordering::SeqCst));
            let _ = fs::remove_file(&lock_path);
            if !two_readers {
                first_task.abort();
                second_task.abort();
                return Err(format!("second auth reader did not join: {reload_count:?}"));
            }

            let first_value = tokio::time::timeout(Duration::from_secs(2), first_task)
                .await
                .map_err(|_| "first auth reader timed out".to_string())?
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
            let second_value = tokio::time::timeout(Duration::from_secs(2), second_task)
                .await
                .map_err(|_| "second auth reader timed out".to_string())?
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
            Ok((reload_count, first_value, second_value))
        });

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&lock_path);
        let (reload_count, first_value, second_value) = result.unwrap();
        assert_eq!(reload_count, Some(2));
        assert_eq!(first_value, second_value);
        assert_eq!(
            first_value,
            Some(Credential::ApiKey {
                key: Some("new-value".to_string()),
                env: None,
            })
        );
    }

    #[test]
    fn new_reuses_unchanged_shared_file_snapshot_without_reloading() {
        let path = temp_auth_path("constructor-snapshot");
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&lock_path);
        write_auth_file(&path, r#"{"provider":{"type":"api_key","key":"stable"}}"#).unwrap();

        let first = AuthStorage::create(path.clone());
        // The unchanged revision lets a second storage instance reuse the
        // shared snapshot. Holding the lock makes an accidental constructor
        // reload fail deterministically instead of merely doing redundant IO.
        fs::write(&lock_path, format!("{}\n", std::process::id())).unwrap();
        let second = AuthStorage::create(path.clone());
        let value = runtime()
            .block_on(second.read("provider", &AuthOperationOptions::default()))
            .unwrap();
        assert_eq!(
            value,
            Some(Credential::ApiKey {
                key: Some("stable".to_string()),
                env: None,
            })
        );
        drop(first);
        drop(second);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&lock_path);
    }

    fn credential_env(name: String) -> BTreeMap<String, String> {
        BTreeMap::from([(name, "provider-scoped-value".to_string())])
    }
}
