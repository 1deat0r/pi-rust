//! Filesystem abstraction for session storage — the Rust counterpart of the
//! `FileSystem` surface in `packages/agent/src/harness/env/*` and the
//! `JsonlSessionRepoFileSystem` pick list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::types::FileError;

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub mtime_ms: u64,
}

/// Filesystem operations required by the JSONL session storage.
pub trait FileSystem: Send + Sync {
    fn absolute_path(&self, path: &str) -> String;
    fn join_path(&self, base: &str, name: &str) -> String;
    fn read_text_file(&self, path: &str) -> Result<String, FileError>;
    fn read_text_lines(&self, path: &str) -> Result<Vec<String>, FileError>;
    fn write_file(&self, path: &str, content: &str) -> Result<(), FileError>;
    fn append_file(&self, path: &str, content: &str) -> Result<(), FileError>;
    fn rename_file(&self, from: &str, to: &str) -> Result<(), FileError>;
    fn file_info(&self, path: &str) -> Result<FileInfo, FileError>;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, FileError>;
    fn exists(&self, path: &str) -> bool;
    fn create_dir(&self, path: &str) -> Result<(), FileError>;
    fn remove(&self, path: &str) -> Result<(), FileError>;
}

/// Real filesystem implementation over tokio's blocking file API surface.
/// The storage layer is async; fs operations here are synchronous and get
/// spawned by the caller where latency matters.
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
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)
            .map_err(|e| FileError::new(format!("append open {path}: {e}")))?;
        f.write_all(content.as_bytes()).map_err(|e| FileError::new(format!("append {path}: {e}")))
    }

    fn rename_file(&self, from: &str, to: &str) -> Result<(), FileError> {
        std::fs::rename(from, to).map_err(|e| FileError::new(format!("rename {from} -> {to}: {e}")))
    }

    fn file_info(&self, path: &str) -> Result<FileInfo, FileError> {
        let meta = std::fs::metadata(path).map_err(|e| FileError::new(format!("metadata {path}: {e}")))?;
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
        for entry in std::fs::read_dir(path).map_err(|e| FileError::new(format!("list_dir {path}: {e}")))? {
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
            std::fs::remove_dir_all(p).map_err(|e| FileError::new(format!("remove dir {path}: {e}")))
        } else {
            std::fs::remove_file(p).map_err(|e| FileError::new(format!("remove file {path}: {e}")))
        }
    }
}

/// Mutable in-memory filesystem used by tests. Mirrors the observable surface
/// the JSONL storage depends on (write/append/rename/mtime).
#[derive(Debug, Clone, Default)]
pub struct MemoryFs {
    pub files: std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    pub mtimes: std::sync::Arc<std::sync::Mutex<BTreeMap<String, u64>>>,
    pub clock: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MemoryFs {
    pub fn new() -> Self {
        Self {
            files: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            mtimes: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            clock: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
    pub fn content(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }
}

impl FileSystem for MemoryFs {
    fn absolute_path(&self, path: &str) -> String {
        Path::new(path).to_string_lossy().into_owned()
    }
    fn join_path(&self, base: &str, name: &str) -> String {
        Path::new(base).join(name).to_string_lossy().into_owned()
    }
    fn read_text_file(&self, path: &str) -> Result<String, FileError> {
        self.files.lock().unwrap().get(path).cloned()
            .ok_or_else(|| FileError::new(format!("read {path}: No such file or directory")))
    }
    fn read_text_lines(&self, path: &str) -> Result<Vec<String>, FileError> {
        let mut lines: Vec<String> = self.read_text_file(path)?.split('\n').map(|s| s.to_string()).collect();
        if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        Ok(lines)
    }
    fn write_file(&self, path: &str, content: &str) -> Result<(), FileError> {
        let ts = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.files.lock().unwrap().insert(path.to_string(), content.to_string());
        self.mtimes.lock().unwrap().insert(path.to_string(), ts + 1);
        Ok(())
    }
    fn append_file(&self, path: &str, content: &str) -> Result<(), FileError> {
        let ts = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut files = self.files.lock().unwrap();
        let entry = files.entry(path.to_string()).or_insert_with(String::new);
        entry.push_str(content);
        drop(files);
        self.mtimes.lock().unwrap().insert(path.to_string(), ts + 1);
        Ok(())
    }
    fn rename_file(&self, from: &str, to: &str) -> Result<(), FileError> {
        let mut files = self.files.lock().unwrap();
        let content = files.remove(from).ok_or_else(|| FileError::new(format!("rename {from}: missing")))?;
        files.insert(to.to_string(), content);
        drop(files);
        let ts = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.mtimes.lock().unwrap().insert(to.to_string(), ts + 1);
        Ok(())
    }
    fn file_info(&self, path: &str) -> Result<FileInfo, FileError> {
        self.files.lock().unwrap().get(path).map(|_| FileInfo {
            mtime_ms: self.mtimes.lock().unwrap().get(path).copied().unwrap_or(1),
        }).ok_or_else(|| FileError::new(format!("metadata {path}: No such file")))
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, FileError> {
        Ok(Vec::new())
    }
    fn exists(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
    fn create_dir(&self, _path: &str) -> Result<(), FileError> {
        Ok(())
    }
    fn remove(&self, path: &str) -> Result<(), FileError> {
        let _ = self.files.lock().unwrap().remove(path);
        Ok(())
    }
}
