//! Session model — port of `packages/agent/src/harness/session/types.ts`
//! (entries, lane records, storage interface) plus the state view in
//! `state.ts`.

use serde::{Deserialize, Serialize};

pub use crate::types::{SessionError, SessionErrorKind};

use crate::types::AgentMessage;

pub type JsonValue = serde_json::Value;

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// Entry union. JSONL files use `type` as the discriminator; `seq`,
/// `parentId`, and `timestamp` are storage-assigned at append time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    #[serde(rename = "message")]
    Message {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        message: AgentMessage,
        #[serde(skip_serializing_if = "Option::is_none")]
        terminate: Option<bool>,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevel {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    #[serde(rename = "active_tools_change")]
    ActiveTools {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        #[serde(rename = "activeToolNames")]
        active_tool_names: Vec<String>,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        summary: String,
        #[serde(rename = "retainedTail")]
        retained_tail: Vec<AgentMessage>,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<pi_ai::types::Usage>,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<pi_ai::types::Usage>,
    },
    #[serde(rename = "custom")]
    Custom {
        id: String,
        seq: u64,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: u64,
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<JsonValue>,
    },
}

impl Entry {
    pub fn from_provisioned(
        entry: EntryNoStats,
        parent_id: Option<String>,
        seq: u64,
        timestamp: u64,
    ) -> Self {
        let pid = parent_id;
        match entry {
            EntryNoStats::Message {
                id,
                message,
                terminate,
            } => Entry::Message {
                id,
                seq,
                parent_id: pid,
                timestamp,
                message,
                terminate,
            },
            EntryNoStats::ModelChange {
                id,
                provider,
                model_id,
            } => Entry::ModelChange {
                id,
                seq,
                parent_id: pid,
                timestamp,
                provider,
                model_id,
            },
            EntryNoStats::ThinkingLevel { id, thinking_level } => Entry::ThinkingLevel {
                id,
                seq,
                parent_id: pid,
                timestamp,
                thinking_level,
            },
            EntryNoStats::ActiveTools {
                id,
                active_tool_names,
            } => Entry::ActiveTools {
                id,
                seq,
                parent_id: pid,
                timestamp,
                active_tool_names,
            },
            EntryNoStats::Compaction {
                id,
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
            } => Entry::Compaction {
                id,
                seq,
                parent_id: pid,
                timestamp,
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
            },
            EntryNoStats::BranchSummary {
                id,
                from_id,
                summary,
                details,
                usage,
            } => Entry::BranchSummary {
                id,
                seq,
                parent_id: pid,
                timestamp,
                from_id,
                summary,
                details,
                usage,
            },
            EntryNoStats::Custom {
                id,
                custom_type,
                data,
            } => Entry::Custom {
                id,
                seq,
                parent_id: pid,
                timestamp,
                custom_type,
                data,
            },
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Entry::Message { id, .. }
            | Entry::ModelChange { id, .. }
            | Entry::ThinkingLevel { id, .. }
            | Entry::ActiveTools { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::BranchSummary { id, .. }
            | Entry::Custom { id, .. } => id,
        }
    }
    pub fn seq(&self) -> u64 {
        match self {
            Entry::Message { seq, .. }
            | Entry::ModelChange { seq, .. }
            | Entry::ThinkingLevel { seq, .. }
            | Entry::ActiveTools { seq, .. }
            | Entry::Compaction { seq, .. }
            | Entry::BranchSummary { seq, .. }
            | Entry::Custom { seq, .. } => *seq,
        }
    }
    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Entry::Message { parent_id, .. }
            | Entry::ModelChange { parent_id, .. }
            | Entry::ThinkingLevel { parent_id, .. }
            | Entry::ActiveTools { parent_id, .. }
            | Entry::Compaction { parent_id, .. }
            | Entry::BranchSummary { parent_id, .. }
            | Entry::Custom { parent_id, .. } => parent_id.as_deref(),
        }
    }
    pub fn timestamp(&self) -> u64 {
        match self {
            Entry::Message { timestamp, .. }
            | Entry::ModelChange { timestamp, .. }
            | Entry::ThinkingLevel { timestamp, .. }
            | Entry::ActiveTools { timestamp, .. }
            | Entry::Compaction { timestamp, .. }
            | Entry::BranchSummary { timestamp, .. }
            | Entry::Custom { timestamp, .. } => *timestamp,
        }
    }
    pub fn entry_type_str(&self) -> &'static str {
        match self {
            Entry::Message { .. } => "message",
            Entry::ModelChange { .. } => "model_change",
            Entry::ThinkingLevel { .. } => "thinking_level_change",
            Entry::ActiveTools { .. } => "active_tools_change",
            Entry::Compaction { .. } => "compaction",
            Entry::BranchSummary { .. } => "branch_summary",
            Entry::Custom { .. } => "custom",
        }
    }
    pub fn as_message(&self) -> Option<&AgentMessage> {
        match self {
            Entry::Message { message, .. } => Some(message),
            _ => None,
        }
    }
    pub fn as_message_terminate(&self) -> Option<bool> {
        match self {
            Entry::Message { terminate, .. } => Some(terminate.unwrap_or(false)),
            _ => None,
        }
    }
    pub fn custom_type_of(&self) -> Option<&str> {
        match self {
            Entry::Custom { custom_type, .. } => Some(custom_type),
            _ => None,
        }
    }
}

/// Provisioned entries lack `seq`/`parentId`/`timestamp` (assigned at append).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryNoStats {
    #[serde(rename = "message")]
    Message {
        id: String,
        message: AgentMessage,
        #[serde(skip_serializing_if = "Option::is_none")]
        terminate: Option<bool>,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        id: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevel {
        id: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    #[serde(rename = "active_tools_change")]
    ActiveTools {
        id: String,
        #[serde(rename = "activeToolNames")]
        active_tool_names: Vec<String>,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: String,
        summary: String,
        #[serde(rename = "retainedTail")]
        retained_tail: Vec<AgentMessage>,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<pi_ai::types::Usage>,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        id: String,
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<pi_ai::types::Usage>,
    },
    #[serde(rename = "custom")]
    Custom {
        id: String,
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<JsonValue>,
    },
}

impl EntryNoStats {
    pub fn id(&self) -> &str {
        match self {
            EntryNoStats::Message { id, .. }
            | EntryNoStats::ModelChange { id, .. }
            | EntryNoStats::ThinkingLevel { id, .. }
            | EntryNoStats::ActiveTools { id, .. }
            | EntryNoStats::Compaction { id, .. }
            | EntryNoStats::BranchSummary { id, .. }
            | EntryNoStats::Custom { id, .. } => id,
        }
    }
}

// ---------------------------------------------------------------------------
// Lane records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaneRecord {
    #[serde(rename = "operation_started")]
    OperationStarted {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(rename = "sourceLeafId")]
        source_leaf_id: Option<String>,
        intent: OperationIntent,
    },
    #[serde(rename = "abort_requested")]
    AbortRequested {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(rename = "runId")]
        run_id: String,
    },
    #[serde(rename = "operation_finished")]
    OperationFinished {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(rename = "runId")]
        run_id: String,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
    },
    #[serde(rename = "step_attempt")]
    StepAttempt {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(rename = "runId")]
        run_id: String,
        step: String,
        attempt: u64,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "compactionReason")]
        compaction_reason: Option<String>,
    },
    #[serde(rename = "tool_started")]
    ToolStarted {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "assistantEntryId")]
        assistant_entry_id: String,
        #[serde(rename = "toolIndex")]
        tool_index: u64,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "effectiveArgs")]
        effective_args: serde_json::Value,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        replay: String,
    },
    #[serde(rename = "queue_enqueued")]
    QueueEnqueued {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        queue: String,
        #[serde(rename = "runId")]
        run_id: String,
        target: serde_json::Value,
    },
    #[serde(rename = "queue_cancelled")]
    QueueCancelled {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "runId")]
        run_id: Option<String>,
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    #[serde(rename = "write_deferred")]
    WriteDeferred {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        #[serde(rename = "runId")]
        run_id: String,
        target: serde_json::Value,
    },
    #[serde(rename = "usage")]
    Usage {
        id: String,
        seq: u64,
        lane: String,
        timestamp: u64,
        cause: String,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        attempt: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "stopReason")]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "toolCallId")]
        tool_call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        usage: pi_ai::types::Usage,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationIntent {
    Run {
        #[serde(rename = "originalPrompt")]
        original_prompt: Vec<AgentMessage>,
        #[serde(rename = "initialMessages")]
        initial_messages: Vec<EntryNoStats>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "systemPromptOverride")]
        system_prompt_override: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "resumeData")]
        resume_data: Option<std::collections::BTreeMap<String, JsonValue>>,
    },
    Compaction {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
    },
    Navigation {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
        summarize: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "summaryEntryId")]
        summary_entry_id: Option<String>,
    },
}

impl LaneRecord {
    pub fn id(&self) -> &str {
        match self {
            LaneRecord::OperationStarted { id, .. }
            | LaneRecord::AbortRequested { id, .. }
            | LaneRecord::OperationFinished { id, .. }
            | LaneRecord::StepAttempt { id, .. }
            | LaneRecord::ToolStarted { id, .. }
            | LaneRecord::QueueEnqueued { id, .. }
            | LaneRecord::QueueCancelled { id, .. }
            | LaneRecord::WriteDeferred { id, .. }
            | LaneRecord::Usage { id, .. } => id,
        }
    }
    pub fn seq(&self) -> u64 {
        match self {
            LaneRecord::OperationStarted { seq, .. }
            | LaneRecord::AbortRequested { seq, .. }
            | LaneRecord::OperationFinished { seq, .. }
            | LaneRecord::StepAttempt { seq, .. }
            | LaneRecord::ToolStarted { seq, .. }
            | LaneRecord::QueueEnqueued { seq, .. }
            | LaneRecord::QueueCancelled { seq, .. }
            | LaneRecord::WriteDeferred { seq, .. }
            | LaneRecord::Usage { seq, .. } => *seq,
        }
    }
    pub fn timestamp(&self) -> u64 {
        match self {
            LaneRecord::OperationStarted { timestamp, .. }
            | LaneRecord::AbortRequested { timestamp, .. }
            | LaneRecord::OperationFinished { timestamp, .. }
            | LaneRecord::StepAttempt { timestamp, .. }
            | LaneRecord::ToolStarted { timestamp, .. }
            | LaneRecord::QueueEnqueued { timestamp, .. }
            | LaneRecord::QueueCancelled { timestamp, .. }
            | LaneRecord::WriteDeferred { timestamp, .. }
            | LaneRecord::Usage { timestamp, .. } => *timestamp,
        }
    }
    pub fn lane(&self) -> &str {
        match self {
            LaneRecord::OperationStarted { lane, .. }
            | LaneRecord::AbortRequested { lane, .. }
            | LaneRecord::OperationFinished { lane, .. }
            | LaneRecord::StepAttempt { lane, .. }
            | LaneRecord::ToolStarted { lane, .. }
            | LaneRecord::QueueEnqueued { lane, .. }
            | LaneRecord::QueueCancelled { lane, .. }
            | LaneRecord::WriteDeferred { lane, .. }
            | LaneRecord::Usage { lane, .. } => lane,
        }
    }
    pub fn record_type(&self) -> &'static str {
        match self {
            LaneRecord::OperationStarted { .. } => "operation_started",
            LaneRecord::AbortRequested { .. } => "abort_requested",
            LaneRecord::OperationFinished { .. } => "operation_finished",
            LaneRecord::StepAttempt { .. } => "step_attempt",
            LaneRecord::ToolStarted { .. } => "tool_started",
            LaneRecord::QueueEnqueued { .. } => "queue_enqueued",
            LaneRecord::QueueCancelled { .. } => "queue_cancelled",
            LaneRecord::WriteDeferred { .. } => "write_deferred",
            LaneRecord::Usage { .. } => "usage",
        }
    }
}

/// New record without storage-assigned seq/timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NewRecord {
    #[serde(rename = "operation_started")]
    OperationStarted {
        id: String,
        lane: String,
        #[serde(rename = "sourceLeafId")]
        source_leaf_id: Option<String>,
        intent: OperationIntent,
    },
    #[serde(rename = "abort_requested")]
    AbortRequested {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
    },
    #[serde(rename = "operation_finished")]
    OperationFinished {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
    },
    #[serde(rename = "step_attempt")]
    StepAttempt {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        step: String,
        attempt: u64,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "compactionReason")]
        compaction_reason: Option<String>,
    },
    #[serde(rename = "tool_started")]
    ToolStarted {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "assistantEntryId")]
        assistant_entry_id: String,
        #[serde(rename = "toolIndex")]
        tool_index: u64,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "effectiveArgs")]
        effective_args: serde_json::Value,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        replay: String,
    },
    #[serde(rename = "queue_enqueued")]
    QueueEnqueued {
        id: String,
        lane: String,
        queue: String,
        #[serde(rename = "runId")]
        run_id: String,
        target: serde_json::Value,
    },
    #[serde(rename = "queue_cancelled")]
    QueueCancelled {
        id: String,
        lane: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "runId")]
        run_id: Option<String>,
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    #[serde(rename = "write_deferred")]
    WriteDeferred {
        id: String,
        lane: String,
        #[serde(rename = "runId")]
        run_id: String,
        target: serde_json::Value,
    },
    #[serde(rename = "usage")]
    Usage {
        id: String,
        lane: String,
        cause: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "runId")]
        run_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "entryId")]
        entry_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        attempt: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "stopReason")]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "toolCallId")]
        tool_call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
        usage: pi_ai::types::Usage,
    },
}

impl NewRecord {
    pub fn id(&self) -> &str {
        match self {
            NewRecord::OperationStarted { id, .. }
            | NewRecord::AbortRequested { id, .. }
            | NewRecord::OperationFinished { id, .. }
            | NewRecord::StepAttempt { id, .. }
            | NewRecord::ToolStarted { id, .. }
            | NewRecord::QueueEnqueued { id, .. }
            | NewRecord::QueueCancelled { id, .. }
            | NewRecord::WriteDeferred { id, .. }
            | NewRecord::Usage { id, .. } => id,
        }
    }
    pub fn lane(&self) -> &str {
        match self {
            NewRecord::OperationStarted { lane, .. }
            | NewRecord::AbortRequested { lane, .. }
            | NewRecord::OperationFinished { lane, .. }
            | NewRecord::StepAttempt { lane, .. }
            | NewRecord::ToolStarted { lane, .. }
            | NewRecord::QueueEnqueued { lane, .. }
            | NewRecord::QueueCancelled { lane, .. }
            | NewRecord::WriteDeferred { lane, .. }
            | NewRecord::Usage { lane, .. } => lane,
        }
    }
    pub fn record_type(&self) -> &'static str {
        match self {
            NewRecord::OperationStarted { .. } => "operation_started",
            NewRecord::AbortRequested { .. } => "abort_requested",
            NewRecord::OperationFinished { .. } => "operation_finished",
            NewRecord::StepAttempt { .. } => "step_attempt",
            NewRecord::ToolStarted { .. } => "tool_started",
            NewRecord::QueueEnqueued { .. } => "queue_enqueued",
            NewRecord::QueueCancelled { .. } => "queue_cancelled",
            NewRecord::WriteDeferred { .. } => "write_deferred",
            NewRecord::Usage { .. } => "usage",
        }
    }
}

// ---------------------------------------------------------------------------
// Mutations, metadata, stats
// ---------------------------------------------------------------------------

/// A line-level mutation stored in the JSONL file (port of
/// `SessionMutation` from `state.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mutation {
    #[serde(rename = "entry")]
    Entry {
        #[serde(skip_serializing_if = "Option::is_none")]
        lane: Option<String>,
        #[serde(flatten)]
        entry: Entry,
    },
    #[serde(rename = "record")]
    Record {
        #[serde(flatten)]
        record: LaneRecord,
    },
    #[serde(rename = "lane")]
    Lane {
        seq: u64,
        lane: String,
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    #[serde(rename = "fact")]
    Fact(Fact),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum Fact {
    #[serde(rename = "name")]
    Name {
        seq: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    #[serde(rename = "label")]
    Label {
        seq: u64,
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

impl Fact {
    pub fn seq(&self) -> u64 {
        match self {
            Fact::Name { seq, .. } => *seq,
            Fact::Label { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonlV4Header {
    #[serde(rename = "kind")]
    pub kind: String, // "header"
    pub version: u64,
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "legacyParentSessionPath")]
    pub legacy_parent_session_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
}

impl Default for JsonlV4Header {
    fn default() -> Self {
        Self {
            kind: "header".into(),
            version: 4,
            id: String::new(),
            created_at: 0,
            cwd: String::new(),
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: u64,
    pub cwd: String,
    pub path: String,
    pub modified_at: u64,
    pub source_format: u64,
    pub parent_session_id: Option<String>,
    pub legacy_parent_session_path: Option<String>,
    pub metadata: Option<JsonValue>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    pub message_count: u64,
    pub cached_tokens: i64,
    pub uncached_tokens: i64,
    pub total_tokens: i64,
    pub cost_total: f64,
}

/// `LogItem` from session/types.ts — the full mutation log union.
#[derive(Debug, Clone, PartialEq)]
pub enum LogItem {
    Entry(Entry),
    Record(LaneRecord),
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    Fact(FactLogItem),
}

/// `LogItem` fact shape (kind "fact", fact "name"|"label").
#[derive(Debug, Clone, PartialEq)]
pub struct FactLogItem {
    pub seq: u64,
    pub fact: String,
    pub name: Option<String>,
    pub target_id: Option<String>,
    pub label: Option<String>,
}

impl LogItem {
    pub fn seq(&self) -> u64 {
        match self {
            LogItem::Entry(e) => e.seq(),
            LogItem::Record(r) => r.seq(),
            LogItem::Lane { seq, .. } => *seq,
            LogItem::Fact(f) => f.seq,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            LogItem::Entry(_) => "entry",
            LogItem::Record(_) => "record",
            LogItem::Lane { .. } => "lane",
            LogItem::Fact(_) => "fact",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LanePointer {
    pub lane: String,
    pub leaf_id: Option<String>,
}

pub fn session_error(kind: SessionErrorKind, message: impl Into<String>) -> SessionError {
    SessionError::new(kind, message)
}

/// Storage-internal helper: rewrite an entry's sequence (used by fork).
pub fn set_entry_seq(entry: &mut Entry, seq: u64) {
    match entry {
        Entry::Message { seq: s, .. }
        | Entry::ModelChange { seq: s, .. }
        | Entry::ThinkingLevel { seq: s, .. }
        | Entry::ActiveTools { seq: s, .. }
        | Entry::Compaction { seq: s, .. }
        | Entry::BranchSummary { seq: s, .. }
        | Entry::Custom { seq: s, .. } => *s = seq,
    }
}
