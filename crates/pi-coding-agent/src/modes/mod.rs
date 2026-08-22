//! Output modes — port of `packages/coding-agent/src/modes/`.
//!
//! `rpc` (JSONL over stdio) is implemented here; `interactive` (the TUI mode)
//! lands with pi-tui.

pub mod jsonl;
pub mod rpc;
pub mod rpc_types;
