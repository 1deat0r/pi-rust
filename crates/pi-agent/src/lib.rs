//! Agent runtime and harness — port of `@earendil-works/pi-agent-core`.
//!
//! Current coverage (P3):
//! - `types.rs`: AgentMessage union (reuses pi-ai messages + custom variant).
//! - `fs.rs`: filesystem abstraction for session storage.
//! - `session/`: entry/record/fact model, in-memory `SessionState`,
//!   JSONL v4 codec + storage (create/load/append/query, torn-tail repair).
//! See PLAN.md P3 and TODO.md for remaining harness work.

pub mod fs;
pub mod session;
pub mod types;

pub use types::AgentMessage;
