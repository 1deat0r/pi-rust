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

/// Custom agent messages — port of the full `CustomAgentMessages` surface
/// from `packages/agent/src/harness/messages.ts` (bashExecution, custom,
/// branchSummary, compactionSummary). Serialized with `role` as the
/// discriminator exactly like upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum CustomAgentMessage {
    #[serde(rename = "bashExecution")]
    BashExecution {
        command: String,
        output: String,
        #[serde(rename = "exitCode")]
        exit_code: Option<i64>,
        cancelled: bool,
        truncated: bool,
        #[serde(rename = "fullOutputPath", skip_serializing_if = "Option::is_none")]
        full_output_path: Option<String>,
        timestamp: u64,
        #[serde(rename = "excludeFromContext", skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
    },
    #[serde(rename = "custom")]
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        content: CustomContent,
        display: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "hookType")]
        hook_type: Option<String>,
        timestamp: u64,
    },
    #[serde(rename = "branchSummary")]
    BranchSummary {
        summary: String,
        #[serde(rename = "fromId")]
        from_id: String,
        timestamp: u64,
    },
    #[serde(rename = "compactionSummary")]
    CompactionSummary {
        summary: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        timestamp: u64,
    },
}

/// `string | (TextContent | ImageContent)[]` for custom message content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomContent {
    String(String),
    Blocks(Vec<pi_ai::types::ContentBlock>),
}

impl CustomAgentMessage {
    pub fn custom_type(&self) -> &str {
        match self {
            CustomAgentMessage::BashExecution { .. } => "bashExecution",
            CustomAgentMessage::Custom { custom_type, .. } => custom_type,
            CustomAgentMessage::BranchSummary { .. } => "branchSummary",
            CustomAgentMessage::CompactionSummary { .. } => "compactionSummary",
        }
    }
    pub fn timestamp(&self) -> u64 {
        match self {
            CustomAgentMessage::BashExecution { timestamp, .. }
            | CustomAgentMessage::Custom { timestamp, .. }
            | CustomAgentMessage::BranchSummary { timestamp, .. }
            | CustomAgentMessage::CompactionSummary { timestamp, .. } => *timestamp,
        }
    }
}


impl AgentMessage {
    pub fn role(&self) -> &'static str {
        match self {
            AgentMessage::Core(m) => m.role(),
            AgentMessage::Custom(c) => c.role(),
        }
    }
}

impl CustomAgentMessage {
    pub fn role(&self) -> &'static str {
        match self {
            CustomAgentMessage::BashExecution { .. } => "bashExecution",
            CustomAgentMessage::Custom { .. } => "custom",
            CustomAgentMessage::BranchSummary { .. } => "branchSummary",
            CustomAgentMessage::CompactionSummary { .. } => "compactionSummary",
        }
    }
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
    InvalidForkTarget,
    InvalidPayload,
    AlreadyExists,
    NotFound,
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
