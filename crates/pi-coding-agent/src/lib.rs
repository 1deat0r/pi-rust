//! Coding agent CLI core — port of `@earendil-works/pi-coding-agent`.
//!
//! Current coverage (P4 milestone): CLI arg parsing with upstream flag
//! surface, config/paths/env resolution, and a `run` path that drives the
//! agent loop against a provider and persists the session JSONL. See TODO.md
//! for the remaining core (settings, model registry/catalog, auth, tools,
//! RPC mode, TUI mode, extensions, compaction).

pub mod args;
pub mod config;
pub mod run;
