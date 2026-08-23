//! JSONL session repository — port of
//! `packages/agent/src/harness/session/jsonl/repo.ts`.

use std::collections::HashSet;

use super::super::session::Session;
use super::super::state::ForkOptions;
use super::super::types::{JsonlV4Header, SessionError, SessionErrorKind, SessionMetadata};
use super::storage::JsonlSessionStorage;
use super::{metadata_from_header, parse_header};
use crate::fs::FileSystem;
use crate::types::FileError;

const SESSION_ID_PATTERN: &str = r"^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$";

fn validate_session_id(id: &str) -> Result<(), SessionError> {
    let re = regex::Regex::new(SESSION_ID_PATTERN).expect("static regex");
    if !re.is_match(id) {
        return Err(SessionError::new(
            SessionErrorKind::InvalidPayload,
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character",
        ));
    }
    Ok(())
}

/// `--<cwd with / and : replaced by ->--`; strips a leading path separator.
pub fn jsonl_session_directory_name(cwd: &str) -> String {
    let stripped = cwd.trim_start_matches(['/', '\\']);
    let replaced: String = stripped
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{replaced}--")
}

pub fn session_file_name(created_at_ms: u64, id: &str) -> String {
    let ts = iso_timestamp_safe(created_at_ms);
    format!("{ts}_{id}.jsonl")
}

fn iso_timestamp_safe(ms: u64) -> String {
    // ISO 8601 UTC with ':' and '.' replaced by '-', mirroring upstream
    // `new Date(createdAt).toISOString().replace(/[:.]/g, "-")`.
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86400;
    let time = secs % 86400;

    let (h, m, s) = (time / 3600, (time / 60) % 60, time % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}-{millis:03}Z")
}

/// Convert days since 1970-01-01 to (year, month, day). Howard Hinnant's
/// civil_from_days algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

pub struct JsonlSessionRepo<F: FileSystem> {
    fs: F,
    sessions_root: String,
    active_create_destinations: HashSet<String>,
}

impl<F: FileSystem> JsonlSessionRepo<F> {
    pub fn new(fs: F, sessions_root: impl Into<String>) -> Self {
        Self {
            fs,
            sessions_root: sessions_root.into(),
            active_create_destinations: HashSet::new(),
        }
    }

    pub async fn create(&mut self, options: CreateOptions) -> Result<Session<F>, SessionError>
    where
        F: Clone,
    {
        let destination = self.resolve_create_destination(options.id.as_deref(), &options.cwd)?;
        let sessions_root = self.sessions_root.clone();
        let fs = self.fs.clone();
        let dest = destination.clone();
        self.claim_create_destination(&destination, async move {
            let (header, path) = prepare_create(&sessions_root, &fs, &dest, &options)?;
            let storage = JsonlSessionStorage::create(fs, &path, header)
                .await
                .map_err(StorageError)?;
            Ok(Session::new(storage))
        })
        .await
    }

    pub async fn open(&self, metadata: &SessionMetadata) -> Result<Session<F>, SessionError>
    where
        F: Clone,
    {
        if !self.fs.exists(&metadata.path) {
            return Err(SessionError::new(
                SessionErrorKind::NotFound,
                format!("Session not found: {}", metadata.id),
            ));
        }
        let storage = JsonlSessionStorage::load(self.fs.clone(), &metadata.path)
            .await
            .map_err(|e| SessionError::new(SessionErrorKind::InvalidEntry, e.to_string()))?;
        let loaded = storage.get_metadata().await;
        if loaded.id != metadata.id {
            return Err(SessionError::new(
                SessionErrorKind::InvalidEntry,
                format!("Session id does not match header: {}", metadata.id),
            ));
        }
        Ok(Session::new(storage))
    }

    pub async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, FileError> {
        list_jsonl_session_metadata(&self.fs, &self.sessions_root, cwd)
    }

    pub async fn delete(&self, metadata: &SessionMetadata) -> Result<(), FileError> {
        self.fs.remove(&metadata.path)
    }

    pub async fn fork(
        &mut self,
        source: &SessionMetadata,
        options: CreateOptions,
    ) -> Result<Session<F>, SessionError>
    where
        F: Clone,
    {
        let source_storage = JsonlSessionStorage::load(self.fs.clone(), &source.path)
            .await
            .map_err(|e| SessionError::new(SessionErrorKind::InvalidEntry, e.to_string()))?;
        let fork_options = options.fork_options.clone();
        let parent_session_id = options
            .parent_session_id
            .clone()
            .or(Some(source.id.clone()));
        let create_options = CreateOptions {
            parent_session_id,
            ..options.clone()
        };
        let destination =
            self.resolve_create_destination(create_options.id.as_deref(), &create_options.cwd)?;
        let sessions_root = self.sessions_root.clone();
        let fs = self.fs.clone();
        let dest = destination.clone();
        self.claim_create_destination(&destination, async move {
            let (header, path) = prepare_create(&sessions_root, &fs, &dest, &create_options)?;
            let storage = source_storage
                .fork(&path, header, &fork_options)
                .await
                .map_err(|e| match e {
                    // Preserve domain errors (invalid_fork_target etc.) instead
                    // of folding them into a generic storage error.
                    super::super::jsonl::storage::ForkError::Session(e) => e,
                    other => SessionError::new(SessionErrorKind::Storage, other.to_string()),
                })?;
            Ok(Session::new(storage))
        })
        .await
    }

    fn resolve_create_destination(
        &self,
        id: Option<&str>,
        cwd: &str,
    ) -> Result<CreateDestination, SessionError> {
        let id = match id {
            Some(id) => id.to_string(),
            None => super::super::session::new_id(),
        };
        validate_session_id(&id)?;
        let cwd = self.fs.absolute_path(cwd);
        Ok(CreateDestination { id, cwd })
    }

    async fn claim_create_destination<T, Fut>(
        &mut self,
        destination: &CreateDestination,
        operation: Fut,
    ) -> Result<T, SessionError>
    where
        Fut: std::future::Future<Output = Result<T, SessionError>>,
    {
        let key = format!("{}\u{0}{}", destination.cwd, destination.id);
        if self.active_create_destinations.contains(&key) {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("Session already exists: {}", destination.id),
            ));
        }
        self.active_create_destinations.insert(key.clone());
        let result = operation.await;
        self.active_create_destinations.remove(&key);
        result
    }
}

fn prepare_create<F: FileSystem>(
    sessions_root: &str,
    fs: &F,
    destination: &CreateDestination,
    options: &CreateOptions,
) -> Result<(JsonlV4Header, String), SessionError> {
    let session_dir = jsonl_session_directory_name(&destination.cwd);
    let dir = fs.join_path(sessions_root, &session_dir);
    if !fs.exists(&dir) {
        fs.create_dir(&dir).map_err(|e| {
            SessionError::new(
                SessionErrorKind::Storage,
                format!("Failed to create sessions directory: {e}"),
            )
        })?;
    }
    // Cross-process/session exclusion: the durable filename ends with
    // `_<id>.jsonl`; reject when that id already exists for this cwd.
    // (Upstream `sessionIdExists`.)
    let suffix = format!("_{}.jsonl", destination.id);
    let exists = fs
        .list_dir_entries(&dir)
        .map(|entries| {
            entries
                .iter()
                .any(|e| !e.is_dir && e.name.ends_with(&suffix))
        })
        .unwrap_or(false);
    if exists {
        return Err(SessionError::new(
            SessionErrorKind::AlreadyExists,
            format!("Session already exists: {}", destination.id),
        ));
    }
    // One shared generation timestamp for the durable filename and header,
    // matching upstream `const createdAt = Date.now()`.
    let created_at = now_ms();
    let path = fs.join_path(&dir, &session_file_name(created_at, &destination.id));
    let header = JsonlV4Header {
        kind: "header".into(),
        version: 4,
        id: destination.id.clone(),
        created_at,
        cwd: destination.cwd.clone(),
        parent_session_id: options.parent_session_id.clone(),
        legacy_parent_session_path: None,
        metadata: options.metadata.clone(),
    };
    Ok((header, path))
}

#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub id: Option<String>,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub fork_options: ForkOptions,
}

impl CreateOptions {
    /// Convenience: set an explicit session id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            id: None,
            cwd: String::new(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Tree,
        }
    }
}

impl CreateOptions {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
struct CreateDestination {
    id: String,
    cwd: String,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "storage error: {}", self.0)
    }
}

/// Marker wrapper to convert storage FileError into SessionError.
#[derive(Debug)]
struct StorageError(FileError);

impl std::error::Error for StorageError {}

impl From<StorageError> for SessionError {
    fn from(e: StorageError) -> Self {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("Failed to initialize session: {}", e.0),
        )
    }
}

/// Lists session metadata across (or within) cwd directories, newest first.
pub fn list_jsonl_session_metadata<F: FileSystem>(
    fs: &F,
    sessions_root: &str,
    cwd: Option<&str>,
) -> Result<Vec<SessionMetadata>, FileError> {
    let root = fs.absolute_path(sessions_root);
    let mut directories: Vec<String> = Vec::new();
    match cwd {
        Some(cwd) => {
            let resolved = fs.absolute_path(cwd);
            let dir = fs.join_path(&root, &jsonl_session_directory_name(&resolved));
            if fs.exists(&dir) {
                directories.push(dir);
            }
        }
        None => {
            if !fs.exists(&root) {
                return Ok(Vec::new());
            }
            for entry in fs.list_dir_entries(&root)? {
                if entry.is_dir || fs.exists(&fs.join_path(&root, &entry.name)) {
                    if entry.name.starts_with("--") && entry.name.ends_with("--") {
                        directories.push(fs.join_path(&root, &entry.name));
                    }
                }
            }
        }
    }
    let mut metadata = Vec::new();
    for directory in directories {
        for entry in fs.list_dir_entries(&directory)? {
            if entry.is_dir || !entry.name.ends_with(".jsonl") {
                continue;
            }
            let path = fs.join_path(&directory, &entry.name);
            let content = fs.read_text_file(&path)?;
            let first_line = content.lines().next().unwrap_or("");
            if first_line.is_empty() {
                continue;
            }
            let Ok(header) = parse_header(first_line) else {
                continue;
            };
            metadata.push(metadata_from_header(&header, &path, entry.mtime_ms));
        }
    }
    metadata.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    Ok(metadata)
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
