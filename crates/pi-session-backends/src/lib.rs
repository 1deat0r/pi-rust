//! pi-session-backends — SQLite session backend.
//!
//! Port of `@earendil-works/pi-session-backend-sqlite-node` (the SQLite
//! implementation of the pi session-backend contract already covered for
//! in-memory and JSONL storage by `pi-agent`).
//!
//! The backend implements the same `Session` facade contract exercised by the
//! shared conformance suite in `crates/pi-agent/tests/conformance.rs`; the
//! sqlite-specific conformance run lives in
//! `crates/pi-session-backends/tests/conformance.rs`.

pub mod branch_cache;
pub mod migrations;
pub mod repo;
pub mod search;
pub mod session;
pub mod sql;
pub mod storage;
pub mod types;

pub use repo::SqliteSessionRepository;
pub use search::create_sqlite_session_search;
pub use session::SqliteSession;
pub use types::{
    SqliteSessionCreateOptions, SqliteSessionListOptions, SqliteSessionMetadata,
    SqliteWriterLeaseOptions,
};

/// Generates a session/branch id (upstream `uuidv7`; pi-agent's port uses
/// uuid v4 too).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
