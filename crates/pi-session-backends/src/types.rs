//! SQLite session backend types — port of
//! `packages/session-backends/sqlite-node/src/sqlite/types.ts`.

use pi_agent::session::types::SessionMetadata;
use serde_json::Value;

/// SQLite session metadata. Extends the core `SessionMetadata` with the
/// SQLite-specific `cwd`, `path`, `name` projection, and application metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct SqliteSessionMetadata {
    pub id: String,
    pub created_at: u64,
    pub cwd: String,
    /// Absolute path of the SQLite database containing this session.
    pub path: String,
    pub parent_session_id: Option<String>,
    /// Current session name projected from SQLite global facts.
    pub name: Option<String>,
    /// Opaque application-owned metadata.
    pub metadata: Option<Value>,
}

impl SqliteSessionMetadata {
    /// Converts to the core metadata shape used by the session facade and the
    /// shared conformance harness.
    pub fn to_core(&self) -> SessionMetadata {
        SessionMetadata {
            id: self.id.clone(),
            created_at: self.created_at,
            cwd: self.cwd.clone(),
            path: self.path.clone(),
            modified_at: self.created_at,
            source_format: 0,
            parent_session_id: self.parent_session_id.clone(),
            legacy_parent_session_path: None,
            metadata: self.metadata.clone(),
        }
    }

    /// Rehydrates from a core metadata shape (harness adapter).
    pub fn from_core(metadata: &SessionMetadata) -> Self {
        Self {
            id: metadata.id.clone(),
            created_at: metadata.created_at,
            cwd: metadata.cwd.clone(),
            path: metadata.path.clone(),
            parent_session_id: metadata.parent_session_id.clone(),
            name: None,
            metadata: metadata.metadata.clone(),
        }
    }
}

/// Create options for a SQLite session.
#[derive(Debug, Clone, Default)]
pub struct SqliteSessionCreateOptions {
    pub id: Option<String>,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub metadata: Option<Value>,
}

/// List options for a SQLite session catalog read.
#[derive(Debug, Clone, Default)]
pub struct SqliteSessionListOptions {
    pub cwd: Option<String>,
}

/// Writer lease timing options.
#[derive(Debug, Clone, Copy)]
pub struct SqliteWriterLeaseOptions {
    /// Time without a successful heartbeat before another writer may take
    /// over. Default: 30 seconds.
    pub ttl_ms: u64,
    /// Idle heartbeat cadence. Default: 10 seconds. Must be less than ttlMs.
    pub heartbeat_interval_ms: u64,
}

impl Default for SqliteWriterLeaseOptions {
    fn default() -> Self {
        Self {
            ttl_ms: 30_000,
            heartbeat_interval_ms: 10_000,
        }
    }
}
