//! Agent message union — port of `packages/agent/src/types.ts` (message part)
//! and the session storage result types.
//!
//! Upstream `AgentMessage = Message | CustomAgentMessages[keyof]`. The custom
//! variants (`hookMessage`/`custom`, role "custom") are defined by
//! `packages/agent/src/harness/messages.ts`; they are carried as
//! `CustomAgentMessage` here and validated on append by the harness.

use pi_ai::types::Message;
use serde::{Deserialize, Serialize};

/// Agent message union (user / assistant / toolResult / custom).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    Core(Message),
    Custom(CustomAgentMessage),
}

/// Custom agent messages (`hookMessage`, `custom` from harness/messages.ts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum CustomAgentMessage {
    #[serde(rename = "custom")]
    Custom {
        #[serde(rename = "type", default = "default_custom_type")]
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
        #[serde(rename = "hookType", skip_serializing_if = "Option::is_none")]
        hook_type: Option<String>,
        timestamp: u64,
    },
}

fn default_custom_type() -> String {
    "custom".to_string()
}

/// Mirrors the upstream `Result<ok, error>` pair used by the session codec.
pub type Result<T, E> = std::result::Result<T, E>;

/// Filesystem errors thrown by storage operations.
#[derive(Debug, Clone, thiserror::Error)]
#[error("filesystem error: {message}")]
pub struct FileError {
    pub message: String,
}

impl FileError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// Session storage error codes (subset of upstream `SessionErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorKind {
    InvalidEntry,
    InvalidQuery,
    InvalidTarget,
    Storage,
    InvalidLane,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("session error {kind:?}: {message}")]
pub struct SessionError {
    pub kind: SessionErrorKind,
    pub message: String,
}

impl SessionError {
    pub fn new(kind: SessionErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}
