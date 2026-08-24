//! Session model and storage — port of
//! `packages/agent/src/harness/session/`.

pub mod context;
pub mod jsonl;
pub mod memory;
#[allow(clippy::module_inception)] // mirrors the upstream session/ directory
pub mod session;
pub mod state;
pub mod types;

pub use jsonl::repo::{
    jsonl_session_directory_name, session_file_name, CreateOptions, JsonlSessionRepo,
};
pub use jsonl::storage::{JsonlSessionStorage, LoadError};
pub use session::{new_id, Session};
pub use state::{
    EntryCursor, EntryOrder, EntryQuery, ForkOptions, ForkPosition, RecordQuery, SessionState,
};
pub use types::*;
