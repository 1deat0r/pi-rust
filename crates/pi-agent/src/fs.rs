//! Filesystem abstraction for session storage — the Rust counterpart of the
//! `FileSystem` surface in `packages/agent/src/harness/env/*` and the
//! `JsonlSessionRepoFileSystem` pick list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::types::FileError;

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub mtime_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub mtime_ms: u64,
}

/// Extension used by fork staging to publish to a sibling temp path and then
/// reload; mirrors upstream's fresh-storage staging which never mutates the
/// source storage's fs.
pub trait FsClone: FileSystem + Clone {}

/// Filesystem operations required by the JSONL session storage.
pub trait FileSystem: Send + Sync {
    /// Clone handle for atomic-publish staging (defaults to cloning if the
    /// concrete fs implements `Clone`; storage callers use `FsClone`).
    fn clone_for_fork(&self) -> Self
    where
        Self: Sized,
        Self: Clone,
    {
        self.clone()
    }
    fn absolute_path(&self, path: &str) -> String;
    fn join_path(&self, base: &str, name: &str) -> String;
    fn read_text_file(&self, path: &str) -> Result<String, FileError>;
    fn read_text_lines(&self, path: &str) -> Result<Vec<String>, FileError>;
    fn write_file(&self, path: &str, content: &str) -> Result<(), FileError>;
    fn append_file(&self, path: &str, content: &str) -> Result<(), FileError>;
    fn rename_file(&self, from: &str, to: &str) -> Result<(), FileError>;
    fn file_info(&self, path: &str) -> Result<FileInfo, FileError>;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, FileError>;

    /// Directory listing with entry kinds and modification times (used by the
    /// session repo for discovery and mtime-ordered listings).
    fn list_dir_entries(&self, path: &str) -> Result<Vec<DirEntry>, FileError> {
        Ok(self
            .list_dir(path)?
            .into_iter()
            .map(|name| DirEntry {
                name,
                is_dir: false,
                mtime_ms: 0,
            })
            .collect())
    }
    fn exists(&self, path: &str) -> bool;
    fn create_dir(&self, path: &str) -> Result<(), FileError>;
    fn remove(&self, path: &str) -> Result<(), FileError>;
}

/// Real filesystem implementation over std::fs. The storage layer is async;
/// fs operations here are synchronous and get spawned by the caller where
/// latency matters.
#[derive(Debug, Clone)]
pub struct StdFileSystem {
    cwd: PathBuf,
}

impl StdFileSystem {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl FileSystem for StdFileSystem {
    fn absolute_path(&self, path: &str) -> String {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_string_lossy().into_owned()
        } else {
            self.cwd.join(p).to_string_lossy().into_owned()
        }
    }

    fn join_path(&self, base: &str, name: &str) -> String {
        Path::new(base).join(name).to_string_lossy().into_owned()
    }

    fn read_text_file(&self, path: &str) -> Result<String, FileError> {
        std::fs::read_to_string(path).map_err(|e| FileError::new(format!("read {path}: {e}")))
    }

    fn read_text_lines(&self, path: &str) -> Result<Vec<String>, FileError> {
        let content = self.read_text_file(path)?;
        let mut lines: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
        if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        Ok(lines)
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), FileError> {
        std::fs::write(path, content).map_err(|e| FileError::new(format!("write {path}: {e}")))
    }

    fn append_file(&self, path: &str, content: &str) -> Result<(), FileError> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| FileError::new(format!("append open {path}: {e}")))?;
        f.write_all(content.as_bytes())
            .map_err(|e| FileError::new(format!("append {path}: {e}")))
    }

    fn rename_file(&self, from: &str, to: &str) -> Result<(), FileError> {
        std::fs::rename(from, to).map_err(|e| FileError::new(format!("rename {from} -> {to}: {e}")))
    }

    fn file_info(&self, path: &str) -> Result<FileInfo, FileError> {
        let meta =
            std::fs::metadata(path).map_err(|e| FileError::new(format!("metadata {path}: {e}")))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(FileInfo { mtime_ms: mtime })
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, FileError> {
        let mut out = Vec::new();
        for entry in
            std::fs::read_dir(path).map_err(|e| FileError::new(format!("list_dir {path}: {e}")))?
        {
            let entry = entry.map_err(|e| FileError::new(format!("list_dir entry {path}: {e}")))?;
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
        out.sort();
        Ok(out)
    }

    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists() || Path::new(path).is_dir()
    }

    fn create_dir(&self, path: &str) -> Result<(), FileError> {
        std::fs::create_dir_all(path).map_err(|e| FileError::new(format!("create_dir {path}: {e}")))
    }

    fn remove(&self, path: &str) -> Result<(), FileError> {
        let p = Path::new(path);
        if p.is_dir() {
            std::fs::remove_dir_all(p)
                .map_err(|e| FileError::new(format!("remove dir {path}: {e}")))
        } else {
            std::fs::remove_file(p).map_err(|e| FileError::new(format!("remove file {path}: {e}")))
        }
    }

    fn list_dir_entries(&self, path: &str) -> Result<Vec<DirEntry>, FileError> {
        let mut out = Vec::new();
        for entry in
            std::fs::read_dir(path).map_err(|e| FileError::new(format!("list_dir {path}: {e}")))?
        {
            let entry = entry.map_err(|e| FileError::new(format!("list_dir entry {path}: {e}")))?;
            let meta = entry.metadata().map_err(|e| {
                FileError::new(format!("list_dir metadata {}: {e}", entry.path().display()))
            })?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                mtime_ms: mtime,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// Mutable in-memory filesystem used by tests. Mirrors the observable surface
/// the JSONL storage depends on (write/append/rename/mtime).
#[derive(Debug, Clone, Default)]
pub struct MemoryFs {
    pub files: std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    pub mtimes: std::sync::Arc<std::sync::Mutex<BTreeMap<String, u64>>>,
    pub dirs: std::sync::Arc<std::sync::Mutex<BTreeSet<String>>>,
    pub clock: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MemoryFs {
    pub fn new() -> Self {
        Self {
            files: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            mtimes: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            dirs: std::sync::Arc::new(std::sync::Mutex::new(BTreeSet::new())),
            clock: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
    pub fn content(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(path)
            .cloned()
    }
    /// Test helper: set a file's modification time directly.
    pub fn set_mtime(&self, path: &str, mtime: u64) {
        self.mtimes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_string(), mtime);
    }
    pub fn ensure_dir(&self, path: &str) {
        self.dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_string());
    }
}

fn path_parent(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}

impl FileSystem for MemoryFs {
    fn absolute_path(&self, path: &str) -> String {
        Path::new(path).to_string_lossy().into_owned()
    }
    fn join_path(&self, base: &str, name: &str) -> String {
        Path::new(base).join(name).to_string_lossy().into_owned()
    }
    fn read_text_file(&self, path: &str) -> Result<String, FileError> {
        self.files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(path)
            .cloned()
            .ok_or_else(|| FileError::new(format!("read {path}: No such file or directory")))
    }
    fn read_text_lines(&self, path: &str) -> Result<Vec<String>, FileError> {
        let mut lines: Vec<String> = self
            .read_text_file(path)?
            .split('\n')
            .map(|s| s.to_string())
            .collect();
        if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        Ok(lines)
    }
    fn write_file(&self, path: &str, content: &str) -> Result<(), FileError> {
        let ts = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_string(), content.to_string());
        self.mtimes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_string(), ts + 1);
        if let Some(parent) = path_parent(path) {
            self.dirs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(parent);
        }
        Ok(())
    }
    fn append_file(&self, path: &str, content: &str) -> Result<(), FileError> {
        let ts = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut files = self.files.lock().unwrap_or_else(|error| error.into_inner());
        let entry = files.entry(path.to_string()).or_default();
        entry.push_str(content);
        drop(files);
        self.mtimes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_string(), ts + 1);
        Ok(())
    }
    fn rename_file(&self, from: &str, to: &str) -> Result<(), FileError> {
        let mut files = self.files.lock().unwrap_or_else(|error| error.into_inner());
        let content = files
            .remove(from)
            .ok_or_else(|| FileError::new(format!("rename {from}: missing")))?;
        files.insert(to.to_string(), content);
        drop(files);
        let ts = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.mtimes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(to.to_string(), ts + 1);
        Ok(())
    }
    fn file_info(&self, path: &str) -> Result<FileInfo, FileError> {
        self.files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(path)
            .map(|_| FileInfo {
                mtime_ms: self
                    .mtimes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(path)
                    .copied()
                    .unwrap_or(1),
            })
            .ok_or_else(|| FileError::new(format!("metadata {path}: No such file")))
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, FileError> {
        Ok(Vec::new())
    }

    fn list_dir_entries(&self, path: &str) -> Result<Vec<DirEntry>, FileError> {
        let prefix = format!("{path}/");
        let files = self.files.lock().unwrap_or_else(|error| error.into_inner());
        let mtimes = self
            .mtimes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut out: Vec<DirEntry> = Vec::new();
        for full in files.keys() {
            if let Some(rel) = full.strip_prefix(&prefix) {
                let name = rel.split('/').next().unwrap_or(rel).to_string();
                if full == &format!("{prefix}{name}") {
                    out.push(DirEntry {
                        mtime_ms: mtimes.get(full).copied().unwrap_or(1),
                        is_dir: false,
                        name,
                    });
                }
            }
        }
        let dirs = self.dirs.lock().unwrap_or_else(|error| error.into_inner());
        for dir in dirs.iter() {
            if let Some(rel) = dir.strip_prefix(&prefix) {
                if let Some(name) = rel.split('/').next() {
                    if !out.iter().any(|e| e.name == name) {
                        out.push(DirEntry {
                            name: name.to_string(),
                            is_dir: true,
                            mtime_ms: 0,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn exists(&self, path: &str) -> bool {
        self.files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(path)
            || self
                .dirs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(path)
    }
    fn create_dir(&self, path: &str) -> Result<(), FileError> {
        {
            let mut dirs = self.dirs.lock().unwrap_or_else(|error| error.into_inner());
            let mut current = Some(path.to_string());
            while let Some(dir) = current {
                dirs.insert(dir.clone());
                current = path_parent(&dir);
            }
        }
        Ok(())
    }
    fn remove(&self, path: &str) -> Result<(), FileError> {
        let _ = self
            .files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(path);
        Ok(())
    }
}
