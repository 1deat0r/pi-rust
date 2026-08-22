//! Execution environment — port of
//! `packages/agent/src/harness/types.ts` (`ExecutionEnv`, `FileSystem`,
//! `Shell`, `FileError`, `ExecutionError`, `Result` helpers) and
//! `packages/agent/src/harness/env/nodejs.ts` (`NodeExecutionEnv`).
//!
//! The upstream implementation is Node-specific (`node:fs/promises`,
//! `node:child_process`, `AbortSignal`). This port implements the same
//! observable contract over `std::fs` + `tokio::process`. Documented
//! divergences:
//! - `AbortSignal` becomes a synchronous `Arc<AtomicBool>` flag. Callers that
//!   cannot hold a `&AbortFlag` across awaits pass `None` (no cancellation).
//! - Cancellation is polled (10 ms interval) instead of event-driven.
//! - The shell is always bash invoked with `-c` (the legacy WSL stdin
//!   transport and Windows Git-Bash discovery are not ported).
//! - Stream chunk boundaries may split UTF-8 differently than Node's
//!   `setEncoding("utf8")` (we pass `from_utf8_lossy` per raw chunk).
//! - `ShellExecOptions` callbacks return `Result<(), String>` so a handler
//!   failure maps to `ExecutionErrorCode::CallbackError` exactly like an
//!   upstream thrown handler (the only way a Rust closure can "throw").

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Result helpers (upstream types.ts ok/err/getOrThrow/getOrUndefined/toError)
// ---------------------------------------------------------------------------

/// Create a successful `Result<T, E>` value.
pub fn ok<T, E>(value: T) -> Result<T, E> {
    Ok(value)
}

/// Create a failed `Result<T, E>` value.
pub fn err<T, E>(error: E) -> Result<T, E> {
    Err(error)
}

/// Return the success value or panic with the failure error (upstream
/// `getOrThrow`, intended for tests and explicit adapter boundaries).
pub fn get_or_throw<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{error}"),
    }
}

/// Return the success value or `None`.
pub fn get_or_undefined<T>(result: Result<T, impl std::fmt::Debug>) -> Option<T> {
    result.ok()
}

/// Normalize unknown thrown values into a printable message (upstream
/// `toError`). Rust errors are never thrown as arbitrary values, so this is
/// a simple `Display` projection used for error causes.
pub fn to_error_message(error: &dyn std::fmt::Display) -> String {
    error.to_string()
}

// ---------------------------------------------------------------------------
// Core error types
// ---------------------------------------------------------------------------

/// Kind of filesystem object as addressed by a `FileSystem`. Symlinks are not
/// followed automatically (upstream `FileKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

impl FileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Directory => "directory",
            FileKind::Symlink => "symlink",
        }
    }
}

/// Stable, backend-independent file error codes returned by `FileSystem` file
/// operations (upstream `FileErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

impl FileErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileErrorCode::Aborted => "aborted",
            FileErrorCode::NotFound => "not_found",
            FileErrorCode::PermissionDenied => "permission_denied",
            FileErrorCode::NotDirectory => "not_directory",
            FileErrorCode::IsDirectory => "is_directory",
            FileErrorCode::Invalid => "invalid",
            FileErrorCode::NotSupported => "not_supported",
            FileErrorCode::Unknown => "unknown",
        }
    }
}

/// Error returned by `FileSystem` file operations (upstream `FileError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    /// Absolute addressed path associated with the failure, when available.
    pub path: Option<String>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<&str>) -> Self {
        Self { code, message: message.into(), path: path.map(|s| s.to_string()) }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": "FileError",
            "code": self.code.as_str(),
            "message": self.message,
            "path": self.path,
        })
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileError({}): {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for FileError {}

/// Stable, backend-independent execution error codes returned by
/// `ExecutionEnv.exec` (upstream `ExecutionErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

impl ExecutionErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionErrorCode::Aborted => "aborted",
            ExecutionErrorCode::Timeout => "timeout",
            ExecutionErrorCode::ShellUnavailable => "shell_unavailable",
            ExecutionErrorCode::SpawnError => "spawn_error",
            ExecutionErrorCode::CallbackError => "callback_error",
            ExecutionErrorCode::Unknown => "unknown",
        }
    }
}

/// Error returned by `ExecutionEnv.exec` (upstream `ExecutionError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": "ExecutionError",
            "code": self.code.as_str(),
            "message": self.message,
        })
    }
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExecutionError({}): {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ExecutionError {}

// ---------------------------------------------------------------------------
// AbortSignal stand-in
// ---------------------------------------------------------------------------

/// Synchronous abort flag — the Rust stand-in for upstream `AbortSignal`
/// (see module docs).
pub type AbortFlag = AtomicBool;

pub fn is_aborted(flag: Option<&AbortFlag>) -> bool {
    flag.map(|f| f.load(Ordering::Relaxed)).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// FileInfo and file contents
// ---------------------------------------------------------------------------

/// Metadata for one filesystem object in a `FileSystem` (upstream
/// `FileInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Basename of `path`.
    pub name: String,
    /// Absolute, syntactically normalized addressed path. Symlinks are not
    /// followed.
    pub path: String,
    /// Object kind.
    pub kind: FileKind,
    /// Size in bytes for the addressed filesystem object.
    pub size: u64,
    /// Modification time as milliseconds since Unix epoch.
    pub mtime_ms: u64,
}

/// Content passed to `writeFile`/`appendFile` (upstream `string | Uint8Array`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl From<String> for FileContent {
    fn from(value: String) -> Self {
        FileContent::Text(value)
    }
}
impl From<&str> for FileContent {
    fn from(value: &str) -> Self {
        FileContent::Text(value.to_string())
    }
}

impl FileContent {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            FileContent::Text(s) => s.as_bytes(),
            FileContent::Bytes(b) => b,
        }
    }
}

// ---------------------------------------------------------------------------
// Options structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReadTextLinesOptions<'a> {
    pub max_lines: Option<usize>,
    pub abort: Option<&'a AbortFlag>,
}

#[derive(Debug, Clone, Copy)]
pub struct CreateDirOptions {
    pub recursive: bool,
}

impl Default for CreateDirOptions {
    fn default() -> Self {
        Self { recursive: true }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub force: bool,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self { recursive: false, force: false }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CreateTempFileOptions<'a> {
    pub prefix: Option<&'a str>,
    pub suffix: Option<&'a str>,
}

/// Stream chunk callback. Returns `Err(message)` to signal a handler failure
/// (mapped to `ExecutionErrorCode::CallbackError`).
pub type ChunkHandler = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Options for `Shell.exec` (upstream `ShellExecOptions`).
pub struct ShellExecOptions {
    /// Working directory for the command. Relative paths are resolved against
    /// the environment cwd. Defaults to the environment cwd.
    pub cwd: Option<String>,
    /// Environment overrides. Values override inherited defaults when
    /// `inherit_env` is true.
    pub env: Option<BTreeMap<String, String>>,
    /// Whether to inherit the execution environment's default variables.
    /// Defaults to true.
    pub inherit_env: bool,
    /// Timeout in seconds. Defaults to no timeout.
    pub timeout: Option<f64>,
    /// Abort flag used to terminate the command. Defaults to no abort flag.
    pub abort: Option<Arc<AbortFlag>>,
    /// Called with stdout chunks as they are produced.
    pub on_stdout: Option<ChunkHandler>,
    /// Called with stderr chunks as they are produced.
    pub on_stderr: Option<ChunkHandler>,
}

impl Default for ShellExecOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            // Upstream default: inherit the environment's default variables.
            inherit_env: true,
            timeout: None,
            abort: None,
            on_stdout: None,
            on_stderr: None,
        }
    }
}

impl std::fmt::Debug for ShellExecOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellExecOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("abort", &self.abort.as_ref().map(|_| "<flag>"))
            .field("on_stdout", &self.on_stdout.as_ref().map(|_| "<handler>"))
            .field("on_stderr", &self.on_stderr.as_ref().map(|_| "<handler>"))
            .finish()
    }
}

/// Result of a shell execution (upstream `{ stdout, stderr, exitCode }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// Filesystem capability used by the harness (upstream `FileSystem`).
///
/// Paths passed to methods may be absolute or relative to `cwd`. Paths
/// returned by file operations are addressed paths in the filesystem
/// namespace, but are not canonicalized through symlinks unless returned by
/// `canonical_path`.
///
/// Operation methods must never panic. All filesystem failures, including
/// unexpected backend failures, are encoded in the returned `Result`.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Current working directory for relative paths.
    fn cwd(&self) -> &str;

    /// Return an absolute addressed path without requiring it to exist and
    /// without resolving symlinks.
    async fn absolute_path(&self, path: &str, abort: Option<&AbortFlag>) -> Result<String, FileError>;
    /// Join path segments in the filesystem namespace without requiring the
    /// result to exist.
    async fn join_path(&self, parts: &[String], abort: Option<&AbortFlag>) -> Result<String, FileError>;
    /// Read a UTF-8 text file.
    async fn read_text_file(&self, path: &str, abort: Option<&AbortFlag>) -> Result<String, FileError>;
    /// Read UTF-8 text lines. Stops once `max_lines` lines have been read.
    async fn read_text_lines(&self, path: &str, options: ReadTextLinesOptions<'_>) -> Result<Vec<String>, FileError>;
    /// Read a binary file.
    async fn read_binary_file(&self, path: &str, abort: Option<&AbortFlag>) -> Result<Vec<u8>, FileError>;
    /// Create or overwrite a file, creating parent directories when
    /// supported.
    async fn write_file(&self, path: &str, content: FileContent, abort: Option<&AbortFlag>)
        -> Result<(), FileError>;
    /// Create or append to a file, creating parent directories when
    /// supported.
    async fn append_file(&self, path: &str, content: FileContent) -> Result<(), FileError>;
    /// Atomically rename a file, replacing the destination when it exists.
    async fn rename_file(&self, source: &str, dest: &str, abort: Option<&AbortFlag>) -> Result<(), FileError>;
    /// Return metadata for the addressed path without following symlinks.
    async fn file_info(&self, path: &str, abort: Option<&AbortFlag>) -> Result<FileInfo, FileError>;
    /// List direct children of a directory without following symlinks.
    async fn list_dir(&self, path: &str, abort: Option<&AbortFlag>) -> Result<Vec<FileInfo>, FileError>;
    /// Return the canonical path for an existing path, resolving symlinks
    /// where supported.
    async fn canonical_path(&self, path: &str, abort: Option<&AbortFlag>) -> Result<String, FileError>;
    /// Return false for missing paths; other errors return a `FileError`.
    async fn exists(&self, path: &str, abort: Option<&AbortFlag>) -> Result<bool, FileError>;
    /// Create a directory. Defaults to `recursive: true`.
    async fn create_dir(&self, path: &str, options: CreateDirOptions) -> Result<(), FileError>;
    /// Remove a file or directory. Defaults to `recursive: false`,
    /// `force: false`.
    async fn remove(&self, path: &str, options: RemoveOptions) -> Result<(), FileError>;
    /// Create a temporary directory and return its absolute path. Defaults to
    /// prefix `"tmp-"`.
    async fn create_temp_dir(&self, prefix: &str, abort: Option<&AbortFlag>) -> Result<String, FileError>;
    /// Create a temporary file and return its absolute path. Defaults to
    /// prefix `""`, suffix `""`.
    async fn create_temp_file(&self, options: CreateTempFileOptions<'_>) -> Result<String, FileError>;

    /// Release filesystem resources. Best-effort; must not panic.
    async fn cleanup(&self);
}

/// Shell execution capability used by the harness (upstream `Shell`).
#[async_trait]
pub trait Shell: Send + Sync {
    /// Execute a shell command in `cwd` unless `options.cwd` is provided.
    async fn exec(&self, command: &str, options: &ShellExecOptions) -> Result<ExecResult, ExecutionError>;
    /// Release shell resources. Best-effort; must not panic.
    async fn cleanup(&self);
}

/// Filesystem and process execution environment used by the harness
/// (upstream `ExecutionEnv`).
#[async_trait]
pub trait ExecutionEnv: FileSystem + Shell {}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Minimal percent-decoder for `file://` URL paths (upstream
/// `fileURLToPath`). `+` is not decoded.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a possibly-relative path against `cwd`, expanding `~`, `~/`, and
/// `file://` URLs (upstream `resolvePath`).
fn resolve_path(cwd: &str, path: &str) -> String {
    let normalized;
    if path == "~" {
        normalized = home_dir();
    } else if let Some(rest) = path.strip_prefix("~/") {
        normalized = format!("{}/{}", home_dir(), rest);
    } else if let Some(url) = path.strip_prefix("file://") {
        // Keep malformed URLs as ordinary paths so filesystem methods preserve
        // their non-throwing contract (upstream behavior).
        normalized = percent_decode(url);
    } else {
        normalized = path.to_string();
    }
    let p = PathBuf::from(&normalized);
    if p.is_absolute() {
        p.to_string_lossy().into_owned()
    } else {
        PathBuf::from(cwd).join(p).to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Error mapping helpers
// ---------------------------------------------------------------------------

fn file_info_from_metadata(path: &str, meta: &std::fs::Metadata) -> Result<FileInfo, FileError> {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Directory
    } else if ft.is_file() {
        FileKind::File
    } else {
        return Err(FileError::new(FileErrorCode::Invalid, "Unsupported file type", Some(path)));
    };
    let name = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(FileInfo { name, path: path.to_string(), kind, size: meta.len(), mtime_ms })
}

fn to_file_error(error: &std::io::Error, fallback_path: Option<&str>) -> FileError {
    let path = fallback_path.map(|s| s.to_string());
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        std::io::ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
        std::io::ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => FileErrorCode::Invalid,
        std::io::ErrorKind::Unsupported => FileErrorCode::NotSupported,
        _ => FileErrorCode::Unknown,
    };
    FileError { code, message: error.to_string(), path }
}

/// Timeout validation (upstream `resolveTimeoutMs`).
fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, ExecutionError> {
    const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
    let Some(v) = timeout else { return Ok(None) };
    if !v.is_finite() || v <= 0.0 {
        return Err(ExecutionError::new(ExecutionErrorCode::Timeout, "Invalid timeout: must be a finite number of seconds"));
    }
    let ms = v * 1000.0;
    if ms > MAX_TIMEOUT_MS {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            format!("Invalid timeout: maximum is {} seconds", MAX_TIMEOUT_MS / 1000.0),
        ));
    }
    Ok(Some(ms as u64))
}

// ---------------------------------------------------------------------------
// StdExecutionEnv — port of NodeExecutionEnv
// ---------------------------------------------------------------------------

/// Real filesystem + shell environment over `std::fs` + `tokio::process`
/// (upstream `NodeExecutionEnv`).
#[derive(Clone)]
pub struct StdExecutionEnv {
    cwd: String,
    shell_path: Option<String>,
    shell_env: Option<BTreeMap<String, String>>,
    active_child_pids: Arc<Mutex<HashSet<u32>>>,
}

impl StdExecutionEnv {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into(), shell_path: None, shell_env: None, active_child_pids: Arc::new(Mutex::new(HashSet::new())) }
    }

    pub fn with_shell_path(cwd: impl Into<String>, shell_path: impl Into<String>) -> Self {
        Self { cwd: cwd.into(), shell_path: Some(shell_path.into()), shell_env: None, active_child_pids: Arc::new(Mutex::new(HashSet::new())) }
    }

    pub fn with_shell_env(cwd: impl Into<String>, shell_env: BTreeMap<String, String>) -> Self {
        Self { cwd: cwd.into(), shell_path: None, shell_env: Some(shell_env), active_child_pids: Arc::new(Mutex::new(HashSet::new())) }
    }

    fn build_env(&self, extra: Option<&BTreeMap<String, String>>, inherit: bool) -> BTreeMap<String, String> {
        if !inherit {
            return extra.cloned().unwrap_or_default();
        }
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        if let Some(base) = &self.shell_env {
            for (k, v) in base {
                env.insert(k.clone(), v.clone());
            }
        }
        if let Some(e) = extra {
            for (k, v) in e {
                env.insert(k.clone(), v.clone());
            }
        }
        env
    }

    fn shell_binary(&self) -> Result<String, ExecutionError> {
        if let Some(path) = &self.shell_path {
            if !Path::new(path).exists() {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::ShellUnavailable,
                    format!("Custom shell path not found: {path}"),
                ));
            }
            return Ok(path.clone());
        }
        if Path::new("/bin/bash").exists() {
            return Ok("/bin/bash".to_string());
        }
        // Fall back to sh like upstream's final fallback.
        Ok("sh".to_string())
    }
}

fn kill_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    // SIGKILL the process group. The child is spawned as its own group
    // leader, so -pid addresses the whole tree (upstream killProcessTree).
    let _ = std::process::Command::new("/bin/kill")
        .args(["-KILL", &format!("-{pid}")])
        .status();
}

struct PipeDrain {
    text: String,
    callback_error: Option<ExecutionError>,
}

async fn drain_pipe<R: tokio::io::AsyncRead + Unpin>(mut reader: R, callback: Option<ChunkHandler>) -> PipeDrain {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    let mut raw: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if let Some(cb) = &callback {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if let Err(message) = cb(&text) {
                        return PipeDrain {
                            text: String::from_utf8_lossy(&raw).into_owned(),
                            callback_error: Some(ExecutionError::new(ExecutionErrorCode::CallbackError, message)),
                        };
                    }
                }
            }
            Err(_) => break,
        }
    }
    PipeDrain { text: String::from_utf8_lossy(&raw).into_owned(), callback_error: None }
}

async fn wait_for_abort(flag: Option<&AbortFlag>) {
    if let Some(flag) = flag {
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    } else {
        std::future::pending::<()>().await;
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(instant) => {
            tokio::time::sleep_until(instant).await;
        }
        None => std::future::pending::<()>().await,
    }
}

#[async_trait]
impl FileSystem for StdExecutionEnv {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str, _abort: Option<&AbortFlag>) -> Result<String, FileError> {
        Ok(resolve_path(&self.cwd, path))
    }

    async fn join_path(&self, parts: &[String], _abort: Option<&AbortFlag>) -> Result<String, FileError> {
        let mut joined = PathBuf::new();
        match parts.split_first() {
            None => return Ok(String::new()),
            Some((first, rest)) => {
                joined.push(first);
                for part in rest {
                    joined.push(part);
                }
            }
        }
        Ok(joined.to_string_lossy().into_owned())
    }

    async fn read_text_file(&self, path: &str, abort: Option<&AbortFlag>) -> Result<String, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if is_aborted(abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
        }
        std::fs::read_to_string(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
    }

    async fn read_text_lines(&self, path: &str, options: ReadTextLinesOptions<'_>) -> Result<Vec<String>, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if is_aborted(options.abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
        }
        if options.max_lines.is_some_and(|m| m == 0) {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))?;
        let mut lines: Vec<String> = Vec::new();
        for (idx, line) in content.split('\n').enumerate() {
            if let Some(max) = options.max_lines {
                if lines.len() >= max {
                    break;
                }
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            lines.push(line.to_string());
            let _ = idx;
        }
        Ok(lines)
    }

    async fn read_binary_file(&self, path: &str, abort: Option<&AbortFlag>) -> Result<Vec<u8>, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if is_aborted(abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
        }
        std::fs::read(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
    }

    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        abort: Option<&AbortFlag>,
    ) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if is_aborted(abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
        }
        if let Some(parent) = Path::new(&resolved).parent() {
            std::fs::create_dir_all(parent).map_err(|e| to_file_error(&e, Some(&resolved)))?;
        }
        if is_aborted(abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
        }
        std::fs::write(&resolved, content.as_bytes()).map_err(|e| to_file_error(&e, Some(&resolved)))
    }

    async fn append_file(&self, path: &str, content: FileContent) -> Result<(), FileError> {
        use std::io::Write;
        let resolved = resolve_path(&self.cwd, path);
        if let Some(parent) = Path::new(&resolved).parent() {
            std::fs::create_dir_all(parent).map_err(|e| to_file_error(&e, Some(&resolved)))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)
            .map_err(|e| to_file_error(&e, Some(&resolved)))?;
        file.write_all(content.as_bytes()).map_err(|e| to_file_error(&e, Some(&resolved)))
    }

    async fn rename_file(&self, source: &str, dest: &str, abort: Option<&AbortFlag>) -> Result<(), FileError> {
        let source = resolve_path(&self.cwd, source);
        let dest = resolve_path(&self.cwd, dest);
        if is_aborted(abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&dest)));
        }
        std::fs::rename(&source, &dest).map_err(|e| to_file_error(&e, Some(&source)))
    }

    async fn file_info(&self, path: &str, _abort: Option<&AbortFlag>) -> Result<FileInfo, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        let meta = std::fs::symlink_metadata(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))?;
        file_info_from_metadata(&resolved, &meta)
    }

    async fn list_dir(&self, path: &str, abort: Option<&AbortFlag>) -> Result<Vec<FileInfo>, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if is_aborted(abort) {
            return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
        }
        let read_dir = std::fs::read_dir(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))?;
        let mut infos: Vec<FileInfo> = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|e| to_file_error(&e, Some(&resolved)))?;
            if is_aborted(abort) {
                return Err(FileError::new(FileErrorCode::Aborted, "aborted", Some(&resolved)));
            }
            let entry_path = entry.path();
            match std::fs::symlink_metadata(&entry_path) {
                Ok(meta) => {
                    let path_str = entry_path.to_string_lossy().into_owned();
                    if let Ok(info) = file_info_from_metadata(&path_str, &meta) {
                        infos.push(info);
                    }
                    // Unsupported file types are skipped (upstream behavior).
                }
                Err(e) => {
                    let path_str = entry_path.to_string_lossy().into_owned();
                    return Err(to_file_error(&e, Some(&path_str)));
                }
            }
        }
        Ok(infos)
    }

    async fn canonical_path(&self, path: &str, _abort: Option<&AbortFlag>) -> Result<String, FileError> {
        let resolved = resolve_path(&self.cwd, path);
        std::fs::canonicalize(&resolved)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| to_file_error(&e, Some(&resolved)))
    }

    async fn exists(&self, path: &str, abort: Option<&AbortFlag>) -> Result<bool, FileError> {
        match self.file_info(path, abort).await {
            Ok(_) => Ok(true),
            Err(e) if e.code == FileErrorCode::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn create_dir(&self, path: &str, options: CreateDirOptions) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        if options.recursive {
            std::fs::create_dir_all(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
        } else {
            std::fs::create_dir(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
        }
    }

    async fn remove(&self, path: &str, options: RemoveOptions) -> Result<(), FileError> {
        let resolved = resolve_path(&self.cwd, path);
        let meta = match std::fs::symlink_metadata(&resolved) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && options.force => return Ok(()),
            Err(e) => return Err(to_file_error(&e, Some(&resolved))),
        };
        if meta.is_dir() && options.recursive {
            std::fs::remove_dir_all(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
        } else if meta.is_dir() {
            std::fs::remove_dir(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
        } else {
            std::fs::remove_file(&resolved).map_err(|e| to_file_error(&e, Some(&resolved)))
        }
    }

    async fn create_temp_dir(&self, prefix: &str, _abort: Option<&AbortFlag>) -> Result<String, FileError> {
        let base = std::env::temp_dir();
        let name = format!("{prefix}{}", uuid::Uuid::new_v4().simple());
        let dir = base.join(name);
        std::fs::create_dir(&dir).map_err(|e| to_file_error(&e, Some(dir.to_string_lossy().as_ref())))?;
        Ok(dir.to_string_lossy().into_owned())
    }

    async fn create_temp_file(&self, options: CreateTempFileOptions<'_>) -> Result<String, FileError> {
        let dir = self.create_temp_dir("tmp-", None).await?;
        let dir = PathBuf::from(dir);
        let file_path = dir.join(format!(
            "{}{}{}",
            options.prefix.unwrap_or(""),
            uuid::Uuid::new_v4().simple(),
            options.suffix.unwrap_or("")
        ));
        std::fs::write(&file_path, b"")
            .map_err(|e| to_file_error(&e, Some(file_path.to_string_lossy().as_ref())))?;
        Ok(file_path.to_string_lossy().into_owned())
    }

    async fn cleanup(&self) {
        let pids: Vec<u32> = self.active_child_pids.lock().unwrap().iter().copied().collect();
        for pid in pids {
            kill_process_group(pid);
        }
        self.active_child_pids.lock().unwrap().clear();
    }
}

#[async_trait]
impl Shell for StdExecutionEnv {
    async fn exec(&self, command: &str, options: &ShellExecOptions) -> Result<ExecResult, ExecutionError> {
        if is_aborted(options.abort.as_deref()) {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        let timeout_ms = resolve_timeout_ms(options.timeout)?;

        let cwd = match &options.cwd {
            Some(rel) => resolve_path(&self.cwd, rel),
            None => self.cwd.clone(),
        };
        if !Path::new(&cwd).is_dir() {
            return Err(ExecutionError::new(
                ExecutionErrorCode::SpawnError,
                format!("Working directory does not exist: {cwd}\nCannot execute bash commands."),
            ));
        }
        let shell = self.shell_binary()?;
        let env = self.build_env(options.env.as_ref(), options.inherit_env);

        let mut child = {
            let mut cmd = tokio::process::Command::new(&shell);
            cmd.arg("-c").arg(command).current_dir(&cwd).stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            if !options.inherit_env {
                // Replace rather than inherit (upstream `{...extraEnv}`).
                cmd.env_clear();
            }
            for (k, v) in &env {
                cmd.env(k, v);
            }
            #[cfg(unix)]
            {
                cmd.process_group(0);
            }
            match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        format!("failed to spawn {shell}: {e}"),
                    ));
                }
            }
        };

        let pid = child.id().unwrap_or(0);
        self.active_child_pids.lock().unwrap().insert(pid);

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let so_cb = options.on_stdout.clone();
        let se_cb = options.on_stderr.clone();
        let so_handle = tokio::spawn(drain_pipe(stdout, so_cb));
        let se_handle = tokio::spawn(drain_pipe(stderr, se_cb));

        let abort_flag = options.abort.clone();
        let deadline = timeout_ms.map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));

        let mut exit_status: Option<std::process::ExitStatus> = None;
        let mut timed_out = false;
        let mut aborted = false;
        {
            let mut wait = Box::pin(child.wait());
            loop {
                tokio::select! {
                    _ = wait_for_abort(abort_flag.as_deref()) => {
                        kill_process_group(pid);
                        aborted = true;
                        break;
                    }
                    _ = wait_for_deadline(deadline) => {
                        kill_process_group(pid);
                        timed_out = true;
                        break;
                    }
                    status = &mut wait => {
                        exit_status = Some(status.map_err(|e| {
                            self.active_child_pids.lock().unwrap().remove(&pid);
                            ExecutionError::new(ExecutionErrorCode::SpawnError, format!("wait failed: {e}"))
                        })?);
                        break;
                    }
                }
            }
        }

        let so = so_handle.await.unwrap_or_else(|_| PipeDrain { text: String::new(), callback_error: None });
        let se = se_handle.await.unwrap_or_else(|_| PipeDrain { text: String::new(), callback_error: None });
        self.active_child_pids.lock().unwrap().remove(&pid);

        if let Some(err) = so.callback_error.or(se.callback_error) {
            return Err(err);
        }
        if timed_out {
            let secs = options.timeout.unwrap_or_default();
            return Err(ExecutionError::new(ExecutionErrorCode::Timeout, format!("timeout:{secs}")));
        }
        if aborted {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        Ok(ExecResult {
            stdout: so.text,
            stderr: se.text,
            exit_code: exit_status.map(|s| s.code().unwrap_or(0)).unwrap_or(0),
        })
    }

    async fn cleanup(&self) {
        FileSystem::cleanup(self).await;
    }
}

impl ExecutionEnv for StdExecutionEnv {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn debug_build_env_direct() {
        let env = StdExecutionEnv::with_shell_env(
            ".".to_string(),
            BTreeMap::from([
                ("PI_SESSION_FILE".to_string(), "/stale/parent.jsonl".to_string()),
                ("PI_CODING_AGENT".to_string(), "true".to_string()),
                ("PI_NODE_ENV_PRESERVED_TEST".to_string(), "preserved".to_string()),
            ]),
        );
        let m = env.build_env(None, true);
        eprintln!("DIRECT BUILD_ENV keys: {:?}", m.keys().collect::<Vec<_>>());
        assert_eq!(m.get("PI_NODE_ENV_PRESERVED_TEST").map(|s| s.as_str()), Some("preserved"));
    }

    fn temp_root() -> String {
        let base = std::env::temp_dir();
        let dir = base.join(format!("pi-env-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn reads_writes_lists_and_removes_files_and_directories() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            assert_eq!(get_or_throw(env.absolute_path("nested/child", None).await), format!("{root}/nested/child"));
            assert_eq!(
                get_or_throw(env.join_path(&[root.clone(), "nested".into(), "child".into()], None).await),
                format!("{root}/nested/child")
            );
            get_or_throw(env.create_dir("nested/child", CreateDirOptions::default()).await);
            get_or_throw(env.write_file("nested/child/file.txt", "hel".into(), None).await);
            get_or_throw(env.append_file("nested/child/file.txt", "lo".into()).await);
            assert_eq!(get_or_throw(env.read_text_file("nested/child/file.txt", None).await), "hello");
            assert_eq!(
                get_or_throw(env.read_text_lines("nested/child/file.txt", ReadTextLinesOptions { max_lines: Some(1), abort: None }).await),
                vec!["hello"]
            );
            assert_eq!(
                String::from_utf8(get_or_throw(env.read_binary_file("nested/child/file.txt", None).await)).unwrap(),
                "hello"
            );

            let entries = get_or_throw(env.list_dir("nested/child", None).await);
            assert_eq!(entries.len(), 1);
            let entry = &entries[0];
            assert_eq!(entry.name, "file.txt");
            assert_eq!(entry.path, format!("{root}/nested/child/file.txt"));
            assert_eq!(entry.kind, FileKind::File);
            assert_eq!(entry.size, 5);
            assert!(entry.mtime_ms > 0);

            assert_eq!(get_or_throw(env.exists("nested/child/file.txt", None).await), true);
            get_or_throw(env.remove("nested/child/file.txt", RemoveOptions::default()).await);
            assert_eq!(get_or_throw(env.exists("nested/child/file.txt", None).await), false);
        });
    }

    #[test]
    fn expands_home_relative_paths_and_file_urls() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let home = home_dir();
            assert_eq!(get_or_throw(env.absolute_path("~/pi-env-test", None).await), format!("{home}/pi-env-test"));
            let file_path = format!("{root}/file with spaces.txt");
            let url = format!("file://{file_path}");
            assert_eq!(get_or_throw(env.absolute_path(&url, None).await), file_path);
        });
    }

    #[test]
    fn returns_file_info_for_files_directories_and_symlinks_without_following() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.create_dir("dir", CreateDirOptions { recursive: true }).await);
            get_or_throw(env.write_file("dir/file.txt", "hello".into(), None).await);
            std::os::unix::fs::symlink(format!("{root}/dir/file.txt"), format!("{root}/file-link")).unwrap();
            std::os::unix::fs::symlink(format!("{root}/dir"), format!("{root}/dir-link")).unwrap();

            let dir_info = get_or_throw(env.file_info("dir", None).await);
            assert_eq!(dir_info.name, "dir");
            assert_eq!(dir_info.path, format!("{root}/dir"));
            assert_eq!(dir_info.kind, FileKind::Directory);

            let file_info = get_or_throw(env.file_info("dir/file.txt", None).await);
            assert_eq!(file_info.name, "file.txt");
            assert_eq!(file_info.kind, FileKind::File);
            assert_eq!(file_info.size, 5);

            let link_info = get_or_throw(env.file_info("file-link", None).await);
            assert_eq!(link_info.name, "file-link");
            assert_eq!(link_info.kind, FileKind::Symlink);
            let dir_link_info = get_or_throw(env.file_info("dir-link", None).await);
            assert_eq!(dir_link_info.kind, FileKind::Symlink);

            let canonical = get_or_throw(env.canonical_path("file-link", None).await);
            assert_eq!(canonical, std::fs::canonicalize(format!("{root}/dir/file.txt")).unwrap().to_string_lossy());
        });
    }

    #[test]
    fn lists_symlinks_as_symlinks() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.write_file("target.txt", "hello".into(), None).await);
            std::os::unix::fs::symlink(format!("{root}/target.txt"), format!("{root}/link.txt")).unwrap();
            let entries = get_or_throw(env.list_dir(".", None).await);
            let mut pairs: Vec<(String, FileKind)> = entries.iter().map(|e| (e.name.clone(), e.kind)).collect();
            pairs.sort();
            assert_eq!(
                pairs,
                vec![("link.txt".to_string(), FileKind::Symlink), ("target.txt".to_string(), FileKind::File)]
            );
        });
    }

    #[test]
    fn stops_reading_text_lines_at_the_requested_limit() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.write_file("file.txt", "one\ntwo\nthree".into(), None).await);
            let lines = get_or_throw(env.read_text_lines("file.txt", ReadTextLinesOptions { max_lines: Some(1), abort: None }).await);
            assert_eq!(lines, vec!["one"]);
        });
    }

    #[test]
    fn returns_file_error_for_missing_paths() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let info = env.file_info("missing.txt", None).await;
            assert!(info.is_err());
            let err = info.unwrap_err();
            assert_eq!(err.code, FileErrorCode::NotFound);
            assert_eq!(err.path, Some(format!("{root}/missing.txt")));
            assert_eq!(get_or_throw(env.exists("missing.txt", None).await), false);
        });
    }

    #[test]
    fn returns_file_error_for_listing_non_directories() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.write_file("file.txt", "hello".into(), None).await);
            let result = env.list_dir("file.txt", None).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code, FileErrorCode::NotDirectory);
        });
    }

    #[test]
    fn appends_to_new_files_and_creates_parent_directories() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.append_file("new/nested/file.txt", "a".into()).await);
            get_or_throw(env.append_file("new/nested/file.txt", "b".into()).await);
            assert_eq!(get_or_throw(env.read_text_file("new/nested/file.txt", None).await), "ab");
        });
    }

    #[test]
    fn atomically_renames_and_replaces_destination() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.write_file("source.txt", "new".into(), None).await);
            get_or_throw(env.write_file("destination.txt", "old".into(), None).await);
            get_or_throw(env.rename_file("source.txt", "destination.txt", None).await);
            assert_eq!(get_or_throw(env.exists("source.txt", None).await), false);
            assert_eq!(get_or_throw(env.read_text_file("destination.txt", None).await), "new");
        });
    }

    #[test]
    fn reports_source_path_when_rename_fails_for_missing_source() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.write_file("destination.txt", "unchanged".into(), None).await);
            let result = env.rename_file("missing-source.txt", "destination.txt", None).await;
            let err = result.unwrap_err();
            assert_eq!(err.code, FileErrorCode::NotFound);
            assert_eq!(err.path, Some(format!("{root}/missing-source.txt")));
            assert_eq!(get_or_throw(env.read_text_file("destination.txt", None).await), "unchanged");
        });
    }

    #[test]
    fn creates_temporary_directories_and_files() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let temp_dir = get_or_throw(env.create_temp_dir("prefix-", None).await);
            assert!(Path::new(&temp_dir).is_dir());
            let temp_file = get_or_throw(env.create_temp_file(CreateTempFileOptions { prefix: Some("p-"), suffix: Some(".txt") }).await);
            assert!(Path::new(&temp_file).is_file());
            assert!(temp_file.ends_with(".txt"));
        });
    }

    #[test]
    fn honors_create_dir_recursive_false_and_remove_options() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let create_result = env.create_dir("missing/child", CreateDirOptions { recursive: false }).await;
            assert!(create_result.is_err());
            assert_eq!(create_result.unwrap_err().code, FileErrorCode::NotFound);

            get_or_throw(env.write_file("dir/child/file.txt", "hello".into(), None).await);
            let remove_dir = env.remove("dir", RemoveOptions { recursive: false, force: false }).await;
            assert!(remove_dir.is_err());
            get_or_throw(env.remove("dir", RemoveOptions { recursive: true, force: false }).await);
            assert_eq!(get_or_throw(env.exists("dir", None).await), false);

            let remove_missing = env.remove("missing", RemoveOptions { recursive: false, force: false }).await;
            assert!(remove_missing.is_err());
            get_or_throw(env.remove("missing", RemoveOptions { recursive: false, force: true }).await);
        });
    }

    #[test]
    fn returns_aborted_results_for_pre_aborted_cancellable_operations() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            get_or_throw(env.write_file("file.txt", "hello".into(), None).await);
            let flag = Arc::new(AtomicBool::new(true));
            let cases: Vec<Result<(), FileError>> = vec![
                env.read_text_file("file.txt", Some(&flag)).await.map(|_| ()),
                env.read_text_lines("file.txt", ReadTextLinesOptions { max_lines: None, abort: Some(&flag) }).await.map(|_| ()),
                env.read_binary_file("file.txt", Some(&flag)).await.map(|_| ()),
                env.write_file("other.txt", "hello".into(), Some(&flag)).await,
                env.rename_file("file.txt", "renamed.txt", Some(&flag)).await,
                env.list_dir(".", Some(&flag)).await.map(|_| ()),
            ];
            for result in cases {
                assert!(result.is_err());
                assert_eq!(result.unwrap_err().code, FileErrorCode::Aborted);
            }
        });
    }

    #[test]
    fn cleanup_is_best_effort() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            FileSystem::cleanup(&env).await;
        });
    }

    #[test]
    fn executes_commands_in_cwd_with_env_overrides() {
        rt().block_on(async {
            let root = std::fs::canonicalize(temp_root()).unwrap().to_string_lossy().into_owned();
            let env = StdExecutionEnv::new(root.clone());
            let mut opts = ShellExecOptions::default();
            opts.env = Some(BTreeMap::from([("NODE_ENV_TEST".to_string(), "ok".to_string())]));
            let result = get_or_throw(env.exec("printf '%s:%s' \"$PWD\" \"$NODE_ENV_TEST\"", &opts).await);
            assert_eq!(result, ExecResult { stdout: format!("{root}:ok"), stderr: String::new(), exit_code: 0 });
        });
    }

    #[test]
    fn applies_string_shell_environment_overrides() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::with_shell_env(
                root.clone(),
                BTreeMap::from([
                    ("PI_SESSION_FILE".to_string(), "/stale/parent.jsonl".to_string()),
                    ("PI_CODING_AGENT".to_string(), "true".to_string()),
                    ("PI_NODE_ENV_PRESERVED_TEST".to_string(), "preserved".to_string()),
                ]),
            );
            let mut opts = ShellExecOptions::default();
            opts.env = Some(BTreeMap::from([("PI_SESSION_FILE".to_string(), String::new())]));
            let result = get_or_throw(
                env.exec(
                    "printf '%s:%s|%s|%s' \"${PI_SESSION_FILE+x}\" \"${PI_SESSION_FILE-}\" \"$PI_CODING_AGENT\" \"$PI_NODE_ENV_PRESERVED_TEST\"",
                    &opts,
                )
                .await,
            );
            eprintln!("DEBUG shell_env: {:?}", env.shell_env);
            eprintln!("DEBUG env result stdout: {:?}", result.stdout);
            let dbg = get_or_throw(
                env.exec("env | sort | grep PI_", &ShellExecOptions::default()).await,
            );
            eprintln!("DEBUG env dump:\n{}", dbg.stdout);
            let full = get_or_throw(
                env.exec("env", &ShellExecOptions::default()).await,
            );
            eprintln!("DEBUG FULL ENV:\n{}", full.stdout);
            eprintln!("DEBUG parent PI_CODING_AGENT: {:?}", std::env::var("PI_CODING_AGENT"));
            eprintln!("DEBUG parent PI_NODE_ENV_PRESERVED_TEST: {:?}", std::env::var("PI_NODE_ENV_PRESERVED_TEST"));
            assert_eq!(result.stdout, "x:|true|preserved");

        });
    }

    #[test]
    fn can_replace_rather_than_inherit_the_default_shell_environment() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::with_shell_env(root.clone(), BTreeMap::from([("PI_CONFIGURED".to_string(), "configured".to_string())]));
            let mut opts = ShellExecOptions::default();
            opts.inherit_env = false;
            opts.env = Some(BTreeMap::from([("PI_EXPLICIT".to_string(), "explicit".to_string())]));
            let result = get_or_throw(
                env.exec(
                    "printf '%s:%s:%s' \"${PI_INHERITED-}\" \"${PI_CONFIGURED-}\" \"${PI_EXPLICIT-}\"",
                    &opts,
                )
                .await,
            );
            assert_eq!(result.stdout, "::explicit");
        });
    }

    #[test]
    fn streams_stdout_and_stderr_chunks() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let stdout = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let mut opts = ShellExecOptions::default();
            let so_sink = stdout.clone();
            let se_sink = stderr.clone();
            opts.on_stdout = Some(Arc::new(move |chunk| { so_sink.lock().unwrap().push_str(chunk); Ok(()) }));
            opts.on_stderr = Some(Arc::new(move |chunk| { se_sink.lock().unwrap().push_str(chunk); Ok(()) }));
            let result = get_or_throw(env.exec("printf out; printf err >&2", &opts).await);
            assert_eq!(result, ExecResult { stdout: "out".into(), stderr: "err".into(), exit_code: 0 });
            assert_eq!(*stdout.lock().unwrap(), "out");
            assert_eq!(*stderr.lock().unwrap(), "err");
        });
    }

    #[test]
    fn reports_a_missing_working_directory_before_spawning() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(format!("{root}/missing"));
            let result = env.exec("printf ok", &ShellExecOptions::default()).await;
            let err = result.unwrap_err();
            assert_eq!(err.code, ExecutionErrorCode::SpawnError);
            assert!(err.message.contains("Working directory does not exist"));
        });
    }

    #[test]
    fn returns_non_zero_exit_codes_as_successful_results() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let result = get_or_throw(env.exec("exit 7", &ShellExecOptions::default()).await);
            assert_eq!(result, ExecResult { stdout: String::new(), stderr: String::new(), exit_code: 7 });
        });
    }

    #[test]
    fn returns_timeout_errors_for_commands_exceeding_the_timeout() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let mut opts = ShellExecOptions::default();
            opts.timeout = Some(0.05);
            let result = env.exec("sleep 5", &opts).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code, ExecutionErrorCode::Timeout);
        });
    }

    #[test]
    fn returns_callback_errors_from_exec_stream_handlers() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let mut opts = ShellExecOptions::default();
            opts.on_stdout = Some(Arc::new(|_| Err("callback failed".to_string())));
            let result = env.exec("printf out", &opts).await;
            let err = result.unwrap_err();
            assert_eq!(err.code, ExecutionErrorCode::CallbackError);
            assert_eq!(err.message, "callback failed");
        });
    }

    #[test]
    fn returns_shell_unavailable_and_spawn_errors() {
        rt().block_on(async {
            let root = temp_root();
            let missing = StdExecutionEnv::with_shell_path(root.clone(), format!("{root}/missing-shell"));
            let result = missing.exec("printf ok", &ShellExecOptions::default()).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code, ExecutionErrorCode::ShellUnavailable);

            let bad_shell_path = format!("{root}/not-executable");
            std::fs::write(&bad_shell_path, "not executable").unwrap();
            let bad = StdExecutionEnv::with_shell_path(root.clone(), bad_shell_path.clone());
            let result = bad.exec("printf ok", &ShellExecOptions::default()).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code, ExecutionErrorCode::SpawnError);
        });
    }

    #[test]
    fn returns_an_aborted_result_for_aborted_commands() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let flag = Arc::new(AtomicBool::new(false));
            let abort_flag = flag.clone();
            let handle = tokio::spawn(async move {
                let mut opts = ShellExecOptions::default();
                opts.abort = Some(abort_flag.clone());
                env.exec("sleep 5", &opts).await
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            flag.store(true, Ordering::Relaxed);
            let result = handle.await.unwrap();
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code, ExecutionErrorCode::Aborted);
        });
    }

    #[test]
    fn cleanup_terminates_active_shell_processes() {
        rt().block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let env_for_task = env.clone();
            let handle = tokio::spawn(async move {
                env_for_task.exec("touch started; sleep 30", &ShellExecOptions::default()).await
            });
            for _ in 0..100 {
                if get_or_throw(env.exists("started", None).await) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(get_or_throw(env.exists("started", None).await), true);
            FileSystem::cleanup(&env).await;
            let result = tokio::time::timeout(Duration::from_secs(3), handle).await.unwrap().unwrap();
            assert!(result.is_ok());
        });
    }
}
