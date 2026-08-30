//! Coding agent CLI core — port of `@earendil-works/pi-coding-agent`.
//!
//! Current coverage (P4 milestone): CLI arg parsing with upstream flag
//! surface, config/paths/env resolution, and a `run` path that drives the
//! agent loop against a provider and persists the session JSONL. See TODO.md
//! for the remaining core (settings, model registry/catalog, auth, tools,
//! RPC mode, TUI mode, extensions, compaction).

pub mod args;
pub mod client;
pub mod commands;
pub mod config;
pub mod core;
pub mod interactive;
pub mod list_models;
pub mod modes;
pub mod run;
pub mod theme;

pub use core::agent_session_runtime::{
    create_agent_session_runtime, default_runtime_factory, AgentSessionRuntime,
    BeforeSessionInvalidateCallback, CreateAgentSessionRuntimeFactory,
    CreateAgentSessionRuntimeOptions, RebindSessionCallback,
};
pub use core::agent_session_services::{
    create_agent_session_from_services, create_agent_session_services, AgentSessionDiagnostic,
    AgentSessionServices, CreateAgentSessionServicesOptions, DiagnosticLevel, ResourceLoader,
};
pub use core::sdk::{
    create_agent_session, AgentSession, CreateAgentSessionOptions, CreateAgentSessionResult,
    SessionManager,
};
