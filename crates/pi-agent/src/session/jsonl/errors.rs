//! JSONL decode errors — port of `packages/agent/src/harness/session/jsonl/errors.ts`.

use crate::types::FileError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlDecodeErrorKind {
    Syntax,
    Schema,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("jsonl decode error ({kind:?}): {message}")]
pub struct JsonlDecodeError {
    pub kind: JsonlDecodeErrorKind,
    pub message: String,
}

impl JsonlDecodeError {
    pub fn new(kind: JsonlDecodeErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// Raise a session-file error enclosing a line number, mirroring
/// `invalidFile` from errors.ts.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid session file {path}!:{line}: {kind:?}: {message}")]
pub struct InvalidSessionFileError {
    pub path: String,
    pub line: usize,
    pub kind: JsonlDecodeErrorKind,
    pub message: String,
}

pub fn file_result<T>(result: Result<T, FileError>, message: &str) -> Result<T, FileError> {
    result.map_err(|_| FileError::new(message.to_string()))
}
