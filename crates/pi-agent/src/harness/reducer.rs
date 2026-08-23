//! Lane-state reducer — port of `packages/agent/src/harness/reducer.ts`.
//!
//! Given a bounded lane recovery slice (open operations, records, entries),
//! `reduce_lane_state` reconstructs the lane's orchestration state — the open
//! operation, in-progress step, tool batch, pending queues, deferred handle,
//! effective configuration — and detects terminal failures. `validate_record_log`
//! first checks the slice for contradictions a single-writer record protocol
//! cannot produce; restore must reject those rather than repair them.

use std::collections::{HashMap, HashSet};

use pi_ai::types::{AssistantMessage, ContentBlock, DeferredHandle, StopReason};

use crate::session::types::{Entry, LaneRecord, OperationIntent};

/// Machine-readable category for a contradiction in a lane's durable recovery
/// slice (upstream `RecordLogCorruptionReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionReason {
    MultipleOpenOperations,
    UnknownOperation,
    RecordAfterFinish,
    NonConsecutiveAttempt,
    InvalidCompactionReason,
    QueueAfterAbort,
    InvalidQueueCancellation,
    InconsistentStep,
    ToolCallMismatch,
    DuplicateToolInvocation,
    ProvisionedEntryMismatch,
    InvalidDeferredHandle,
}

impl std::fmt::Display for CorruptionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CorruptionReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CorruptionReason::MultipleOpenOperations => "multiple_open_operations",
            CorruptionReason::UnknownOperation => "unknown_operation",
            CorruptionReason::RecordAfterFinish => "record_after_finish",
            CorruptionReason::NonConsecutiveAttempt => "non_consecutive_attempt",
            CorruptionReason::InvalidCompactionReason => "invalid_compaction_reason",
            CorruptionReason::QueueAfterAbort => "queue_after_abort",
            CorruptionReason::InvalidQueueCancellation => "invalid_queue_cancellation",
            CorruptionReason::InconsistentStep => "inconsistent_step",
            CorruptionReason::ToolCallMismatch => "tool_call_mismatch",
            CorruptionReason::DuplicateToolInvocation => "duplicate_tool_invocation",
            CorruptionReason::ProvisionedEntryMismatch => "provisioned_entry_mismatch",
            CorruptionReason::InvalidDeferredHandle => "invalid_deferred_handle",
        }
    }
}

/// Error thrown when a lane recovery slice is corrupt (upstream
/// `RecordLogCorruption`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("record log corruption ({reason}): {message}")]
pub struct RecordLogCorruption {
    pub reason: CorruptionReason,
    pub message: String,
}

impl RecordLogCorruption {
    pub fn new(reason: CorruptionReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// Bounded lane recovery slice (upstream `RecordLogSlice`).
pub struct RecordLogSlice<'a> {
    pub lane: &'a str,
    pub open_operations: &'a [LaneRecord],
    pub records: &'a [LaneRecord],
    /// Operation-owned entries plus entries fetched by provisioned ids.
    pub entries: &'a [Entry],
}

/// The currently effective model/thinking/tools configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectiveLaneConfiguration {
    pub provider: String,
    pub model_id: String,
    pub thinking_level: String,
    pub active_tool_names: Vec<String>,
}

impl EffectiveLaneConfiguration {
    pub fn new(
        provider: impl Into<String>,
        model_id: impl Into<String>,
        thinking_level: impl Into<String>,
        active_tool_names: Vec<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model_id: model_id.into(),
            thinking_level: thinking_level.into(),
            active_tool_names,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalFailureSource {
    Step,
    DeferredFetch,
}

/// A terminal assistant error produced by the open operation.
#[derive(Debug, Clone)]
pub struct TerminalFailureState {
    pub entry_id: String,
    pub source: TerminalFailureSource,
    pub message: AssistantMessage,
}

/// One tool call within the open assistant entry's batch.
#[derive(Debug, Clone)]
pub struct ToolCallState {
    pub tool_index: u64,
    pub tool_call: ContentBlock,
    pub started: Option<LaneRecord>,
    pub result_exists: bool,
    pub terminate: bool,
}

/// The open assistant entry plus its tool-call batch state.
#[derive(Debug, Clone)]
pub struct ToolBatchState {
    pub assistant_entry_id: String,
    pub calls: Vec<ToolCallState>,
    pub truncated: bool,
    pub unresolved: bool,
}

/// The open operation's in-progress step (only when its result entry has not
/// yet been persisted).
#[derive(Debug, Clone)]
pub struct StepState {
    pub kind: String,
    pub attempts: u64,
    pub result_entry_id: String,
    pub compaction_reason: Option<String>,
}

/// The newest entry appended by the open operation.
#[derive(Debug, Clone)]
pub struct NewestOwn {
    pub entry_id: String,
    pub entry_type: String,
    pub role: Option<String>,
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Clone, Default)]
pub struct OperationTargets {
    pub result: bool,
    pub summary: bool,
}

/// Fully reconstructed orchestration state for one lane.
#[derive(Debug, Clone)]
pub struct LaneState {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub operation: Option<Box<LaneOperationState>>,
    pub pending_next_run: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct LaneOperationState {
    pub id: String,
    pub kind: String,
    pub aborting: bool,
    pub step: Option<StepState>,
    pub tool_batch: Option<ToolBatchState>,
    pub missing_initial_messages: Vec<serde_json::Value>,
    pub pending_steer: Vec<serde_json::Value>,
    pub pending_follow_up: Vec<serde_json::Value>,
    pub pending_writes: Vec<serde_json::Value>,
    pub deferred: Option<DeferredHandle>,
    pub overflow_recovery_used: bool,
    pub newest_own: Option<NewestOwn>,
    pub targets: OperationTargets,
}

/// Lane reduction inputs (upstream `LaneReductionInput`).
pub struct LaneReductionInput<'a> {
    pub lane: &'a str,
    pub leaf_id: Option<&'a str>,
    pub open_operations: &'a [LaneRecord],
    pub records: &'a [LaneRecord],
    pub entries: &'a [Entry],
    /// Entries appended by the open operation, oldest first. Empty when idle.
    pub own_entries: &'a [Entry],
    /// Bounded effective-state lookups at the anchor or idle leaf, oldest
    /// first.
    pub configuration_entries: &'a [Entry],
    /// Harness option fallbacks used when no persisted value exists.
    pub defaults: EffectiveLaneConfiguration,
}

#[derive(Debug, Clone)]
pub struct LaneReductionResult {
    pub lane_state: LaneState,
    pub effective_configuration: EffectiveLaneConfiguration,
    pub terminal_failure: Option<TerminalFailureState>,
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

pub fn entry_id(entry: &Entry) -> &str {
    match entry {
        Entry::Message { id, .. }
        | Entry::ModelChange { id, .. }
        | Entry::ThinkingLevel { id, .. }
        | Entry::ActiveTools { id, .. }
        | Entry::Compaction { id, .. }
        | Entry::BranchSummary { id, .. }
        | Entry::Custom { id, .. } => id.as_str(),
    }
}

pub fn entry_seq(entry: &Entry) -> u64 {
    match entry {
        Entry::Message { seq, .. }
        | Entry::ModelChange { seq, .. }
        | Entry::ThinkingLevel { seq, .. }
        | Entry::ActiveTools { seq, .. }
        | Entry::Compaction { seq, .. }
        | Entry::BranchSummary { seq, .. }
        | Entry::Custom { seq, .. } => *seq,
    }
}

pub fn record_id(record: &LaneRecord) -> &str {
    match record {
        LaneRecord::OperationStarted { id, .. }
        | LaneRecord::AbortRequested { id, .. }
        | LaneRecord::OperationFinished { id, .. }
        | LaneRecord::StepAttempt { id, .. }
        | LaneRecord::ToolStarted { id, .. }
        | LaneRecord::QueueEnqueued { id, .. }
        | LaneRecord::QueueCancelled { id, .. }
        | LaneRecord::WriteDeferred { id, .. }
        | LaneRecord::Usage { id, .. } => id.as_str(),
    }
}

pub fn record_seq(record: &LaneRecord) -> u64 {
    match record {
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

/// Records carrying a `runId`: everything except operation_started and
/// queue_cancelled (the latter carries an optional run id).
/// Extract the inner `AssistantMessage` from an `AgentMessage`.
pub fn as_assistant(message: &crate::types::AgentMessage) -> Option<&AssistantMessage> {
    match message {
        crate::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => Some(a),
        _ => None,
    }
}

/// Extract the inner `ToolResultMessage` from an `AgentMessage`.
pub fn as_tool_result(
    message: &crate::types::AgentMessage,
) -> Option<&pi_ai::types::ToolResultMessage> {
    match message {
        crate::types::AgentMessage::Core(pi_ai::types::Message::ToolResult(tr)) => Some(tr),
        _ => None,
    }
}

fn record_run_id(record: &LaneRecord) -> Option<&str> {
    match record {
        LaneRecord::AbortRequested { run_id, .. }
        | LaneRecord::OperationFinished { run_id, .. }
        | LaneRecord::StepAttempt { run_id, .. }
        | LaneRecord::ToolStarted { run_id, .. }
        | LaneRecord::QueueEnqueued { run_id, .. }
        | LaneRecord::WriteDeferred { run_id, .. }
        | LaneRecord::Usage { run_id, .. } => Some(run_id),
        LaneRecord::OperationStarted { .. } | LaneRecord::QueueCancelled { .. } => None,
    }
}

/// Serialize an entry with storage-assigned fields removed, matching the
/// upstream `Omit<TEntry, "parentId" | "seq" | "timestamp">` projection used
/// by provisioned-entry comparisons.
fn provisioned_entry_json(entry: &Entry) -> serde_json::Value {
    let mut value = serde_json::to_value(entry).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("parentId");
        map.remove("seq");
        map.remove("timestamp");
    }
    value
}

/// Compare a stored entry against a provisioned target (deep equality on the
/// projected payload, like upstream `Guard.IsDeepEqual`).
fn matches_provisioned_entry(entry: &Entry, target: &serde_json::Value) -> bool {
    &provisioned_entry_json(entry) == target
}

/// Provisioned-entry target from an `OperationIntent::Run.initialMessages`
/// entry (already in the provisioned shape — without seq/parentId/timestamp).
fn provisioned_value_from_entry_no_stats(
    entry: &crate::session::types::EntryNoStats,
) -> serde_json::Value {
    serde_json::to_value(entry).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_exact_provisioned_entry(
    entries_by_id: &HashMap<&str, &Entry>,
    target: &serde_json::Value,
) -> Result<(), RecordLogCorruption> {
    let id = target.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(entry) = entries_by_id.get(id) {
        if !matches_provisioned_entry(entry, target) {
            return Err(RecordLogCorruption::new(
                CorruptionReason::ProvisionedEntryMismatch,
                format!("Provisioned entry {id} exists with content different from its intent"),
            ));
        }
    }
    Ok(())
}

fn validate_result_entry(
    entries_by_id: &HashMap<&str, &Entry>,
    result_entry_id: &str,
    matches: &dyn Fn(&Entry) -> bool,
    description: &str,
) -> Result<(), RecordLogCorruption> {
    if let Some(entry) = entries_by_id.get(result_entry_id) {
        if !matches(entry) {
            return Err(RecordLogCorruption::new(
                CorruptionReason::ProvisionedEntryMismatch,
                format!("Provisioned {description} entry {result_entry_id} exists with different content"),
            ));
        }
    }
    Ok(())
}

fn validate_attempt_reason(record: &LaneRecord) -> Result<(), RecordLogCorruption> {
    let LaneRecord::StepAttempt {
        step,
        compaction_reason,
        id,
        ..
    } = record
    else {
        return Ok(());
    };
    let reason = compaction_reason.as_deref();
    if step == "compaction" {
        if !matches!(
            reason,
            Some("manual") | Some("threshold") | Some("overflow")
        ) {
            return Err(RecordLogCorruption::new(
                CorruptionReason::InvalidCompactionReason,
                format!("Compaction attempt {id} has no valid compaction reason"),
            ));
        }
    } else if reason.is_some() {
        return Err(RecordLogCorruption::new(
            CorruptionReason::InvalidCompactionReason,
            format!("{step} attempt {id} has a compaction reason"),
        ));
    }
    Ok(())
}

struct AttemptSeries {
    record: LaneRecord,
}

fn validate_attempt_sequence(
    record: &LaneRecord,
    previous: Option<&AttemptSeries>,
    entries_by_id: &HashMap<&str, &Entry>,
) -> Result<(), RecordLogCorruption> {
    let LaneRecord::StepAttempt {
        step,
        attempt,
        result_entry_id,
        compaction_reason,
        id,
        seq,
        ..
    } = record
    else {
        return Ok(());
    };
    let previous_record = previous.map(|p| &p.record);
    let previous_result = previous_record.and_then(|pr| match pr {
        LaneRecord::StepAttempt {
            result_entry_id, ..
        } => entries_by_id.get(result_entry_id.as_str()).copied(),
        _ => None,
    });
    let continues_series = match previous_record {
        Some(LaneRecord::StepAttempt {
            step: prev_step, ..
        }) => {
            prev_step == step
                && previous_result
                    .map(|e| entry_seq(e) >= *seq)
                    .unwrap_or(true)
        }
        _ => false,
    };
    let previous_attempt = match previous_record {
        Some(LaneRecord::StepAttempt { attempt, .. }) => *attempt,
        _ => 0,
    };
    let expected_attempt = if continues_series {
        previous_attempt + 1
    } else {
        1
    };
    if *attempt != expected_attempt {
        return Err(RecordLogCorruption::new(
            CorruptionReason::NonConsecutiveAttempt,
            format!("{step} attempt {id} is {attempt}; expected {expected_attempt}"),
        ));
    }
    if !continues_series || step == "assistant" || previous_record.is_none() {
        return Ok(());
    }
    if let Some(LaneRecord::StepAttempt {
        result_entry_id: prev_result,
        compaction_reason: prev_reason,
        ..
    }) = previous_record
    {
        if result_entry_id != prev_result {
            return Err(RecordLogCorruption::new(
                CorruptionReason::InconsistentStep,
                format!("{step} attempts disagree on their result entry id"),
            ));
        }
        if compaction_reason != prev_reason {
            return Err(RecordLogCorruption::new(
                CorruptionReason::InconsistentStep,
                format!("{step} attempts disagree on their compaction reason"),
            ));
        }
    }
    Ok(())
}

fn validate_attempt_result(
    entries_by_id: &HashMap<&str, &Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogCorruption> {
    let LaneRecord::StepAttempt {
        step,
        result_entry_id,
        ..
    } = record
    else {
        return Ok(());
    };
    let matches: &dyn Fn(&Entry) -> bool = match step.as_str() {
        "assistant" => {
            &(|entry: &Entry| matches!(entry, Entry::Message { message, .. } if as_assistant(message).is_some()))
        }
        "compaction" => &(|entry: &Entry| matches!(entry, Entry::Compaction { .. })),
        "branch_summary" => &(|entry: &Entry| matches!(entry, Entry::BranchSummary { .. })),
        _ => return Ok(()),
    };
    validate_result_entry(entries_by_id, result_entry_id, matches, "step result")
}

fn validate_tool_start(
    record: &LaneRecord,
    entries_by_id: &HashMap<&str, &Entry>,
    invocations: &mut HashSet<String>,
) -> Result<(), RecordLogCorruption> {
    let LaneRecord::ToolStarted {
        assistant_entry_id,
        tool_index,
        tool_call_id,
        tool_name,
        result_entry_id,
        id,
        ..
    } = record
    else {
        return Ok(());
    };
    let invocation = format!("{assistant_entry_id}\u{0}{tool_index}");
    if !invocations.insert(invocation.clone()) {
        return Err(RecordLogCorruption::new(
            CorruptionReason::DuplicateToolInvocation,
            format!("Tool invocation {assistant_entry_id}:{tool_index} is duplicated"),
        ));
    }

    let assistant_entry = entries_by_id.get(assistant_entry_id.as_str()).copied();
    let Some(Entry::Message { message, .. }) = assistant_entry else {
        return Err(RecordLogCorruption::new(
            CorruptionReason::ToolCallMismatch,
            format!("Tool start {id} does not reference an assistant entry"),
        ));
    };
    let Some(assistant) = as_assistant(message) else {
        return Err(RecordLogCorruption::new(
            CorruptionReason::ToolCallMismatch,
            format!("Tool start {id} does not reference an assistant entry"),
        ));
    };
    let tool_calls: Vec<&ContentBlock> = assistant
        .content()
        .iter()
        .filter(|c| matches!(c, ContentBlock::ToolCall { .. }))
        .collect();
    let tool_index_usize = *tool_index as usize;
    match tool_calls.get(tool_index_usize) {
        Some(ContentBlock::ToolCall {
            id: tc_id,
            name: tc_name,
            ..
        }) if tc_id == tool_call_id && tc_name == tool_name => {}
        _ => {
            return Err(RecordLogCorruption::new(
                CorruptionReason::ToolCallMismatch,
                format!("Tool start {id} does not match its assistant tool-call ordinal"),
            ));
        }
    }

    validate_result_entry(
        entries_by_id,
        result_entry_id,
        &|entry: &Entry| {
            matches!(
                entry,
                Entry::Message { message, .. }
                    if as_tool_result(message)
                        .map(|tr| tr.tool_call_id() == tool_call_id.as_str() && tr.tool_name() == tool_name.as_str())
                        .unwrap_or(false)
            )
        },
        "tool result",
    )
}

fn validate_deferred_handles<'a>(
    entries: impl Iterator<Item = &'a Entry>,
) -> Result<(), RecordLogCorruption> {
    for entry in entries {
        if let Entry::Message { id, message, .. } = entry {
            if message.role() == "assistant"
                && as_assistant(message)
                    .map(|a| a.stop_reason())
                    .unwrap_or(None)
                    == Some(StopReason::Deferred)
                && as_assistant(message).and_then(|a| a.deferred()).is_none()
            {
                return Err(RecordLogCorruption::new(
                    CorruptionReason::InvalidDeferredHandle,
                    format!("Deferred assistant entry {id} does not carry a handle"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_operation_result(
    entries_by_id: &HashMap<&str, &Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogCorruption> {
    let LaneRecord::OperationStarted { intent, .. } = record else {
        return Ok(());
    };
    match intent {
        OperationIntent::Run {
            initial_messages, ..
        } => {
            for target in initial_messages {
                let target_value = provisioned_value_from_entry_no_stats(target);
                validate_exact_provisioned_entry(entries_by_id, &target_value)?;
            }
        }
        OperationIntent::Compaction {
            result_entry_id, ..
        } => {
            validate_result_entry(
                entries_by_id,
                result_entry_id,
                &|entry: &Entry| matches!(entry, Entry::Compaction { .. }),
                "manual compaction",
            )?;
        }
        OperationIntent::Navigation {
            summary_entry_id, ..
        } => {
            if let Some(summary_id) = summary_entry_id {
                validate_result_entry(
                    entries_by_id,
                    summary_id,
                    &|entry: &Entry| matches!(entry, Entry::BranchSummary { .. }),
                    "navigation summary",
                )?;
            }
        }
    }
    Ok(())
}

/// Validate a bounded lane recovery slice without reading or mutating session
/// state (upstream `validateRecordLog`).
pub fn validate_record_log(input: &RecordLogSlice) -> Result<(), RecordLogCorruption> {
    if input.open_operations.len() > 1 {
        return Err(RecordLogCorruption::new(
            CorruptionReason::MultipleOpenOperations,
            format!("Lane {} has at least two open operations", input.lane),
        ));
    }

    let entries_by_id: HashMap<&str, &Entry> =
        input.entries.iter().map(|e| (entry_id(e), e)).collect();
    validate_deferred_handles(input.entries.iter())?;
    let mut starts: HashMap<String, &LaneRecord> = HashMap::new();
    let mut finished_at: HashMap<String, u64> = HashMap::new();
    let mut aborted_at: HashMap<String, u64> = HashMap::new();
    let mut queue_enqueues: HashMap<String, &LaneRecord> = HashMap::new();
    let mut latest_attempt: HashMap<String, AttemptSeries> = HashMap::new();
    let mut tool_invocations: HashSet<String> = HashSet::new();
    let mut records: Vec<&LaneRecord> = input.records.iter().collect();
    records.sort_by_key(|r| record_seq(r));

    for record in records {
        if let LaneRecord::OperationStarted { .. } = record {
            let id = record_id(record).to_string();
            starts.insert(id.clone(), record);
            validate_operation_result(&entries_by_id, record)?;
            continue;
        }

        if let Some(run_id) = record_run_id(record) {
            if !starts.contains_key(run_id) {
                return Err(RecordLogCorruption::new(
                    CorruptionReason::UnknownOperation,
                    format!(
                        "Record {} references unknown operation {run_id}",
                        record_id(record)
                    ),
                ));
            }
            let finish_seq = finished_at.get(run_id).copied();
            if let Some(finish_seq) = finish_seq {
                if record_seq(record) > finish_seq {
                    return Err(RecordLogCorruption::new(
                        CorruptionReason::RecordAfterFinish,
                        format!(
                            "Record {} follows the finish of operation {run_id}",
                            record_id(record)
                        ),
                    ));
                }
            }
        }

        match record {
            LaneRecord::OperationFinished { run_id, seq, .. } => {
                finished_at.insert(run_id.clone(), *seq);
            }
            LaneRecord::AbortRequested { run_id, seq, .. } => {
                aborted_at.insert(run_id.clone(), *seq);
            }
            LaneRecord::StepAttempt { .. } => {
                let run_of_record = record_run_id(record).unwrap_or("").to_string();
                validate_attempt_reason(record)?;
                validate_attempt_sequence(
                    record,
                    latest_attempt.get(&run_of_record),
                    &entries_by_id,
                )?;
                validate_attempt_result(&entries_by_id, record)?;
                latest_attempt.insert(
                    run_of_record,
                    AttemptSeries {
                        record: record.clone(),
                    },
                );
            }
            LaneRecord::ToolStarted { .. } => {
                validate_tool_start(record, &entries_by_id, &mut tool_invocations)?;
            }
            LaneRecord::QueueEnqueued {
                queue,
                run_id,
                target,
                seq,
                ..
            } => {
                if queue != "nextRun" {
                    if let Some(abort_seq) = aborted_at.get(run_id) {
                        if *seq > *abort_seq {
                            return Err(RecordLogCorruption::new(
                                CorruptionReason::QueueAfterAbort,
                                format!(
                                    "{queue} item {} was enqueued after abort",
                                    target.get("id").and_then(|v| v.as_str()).unwrap_or("")
                                ),
                            ));
                        }
                    }
                }
                let target_id = target
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                queue_enqueues.insert(target_id, record);
                validate_exact_provisioned_entry(&entries_by_id, target)?;
            }
            LaneRecord::QueueCancelled {
                entry_id,
                seq,
                run_id,
                ..
            } => {
                let enqueue = queue_enqueues.get(entry_id.as_str()).copied();
                // Upstream: corrupt unless a pending matching enqueue exists
                // (runId equality; an undefined cancel runId matches nothing
                // carrying a runId) and no entry was materialized.
                let enqueue_ok = match enqueue {
                    Some(LaneRecord::QueueEnqueued {
                        seq: enq_seq,
                        run_id: enq_run,
                        ..
                    }) => {
                        enq_run == run_id.as_deref().unwrap_or("")
                            && *enq_seq < *seq
                            && !entries_by_id.contains_key(entry_id.as_str())
                    }
                    _ => false,
                };
                if !enqueue_ok {
                    return Err(RecordLogCorruption::new(
                        CorruptionReason::InvalidQueueCancellation,
                        format!(
                            "Queue cancellation {} has no pending matching enqueue",
                            record_id(record)
                        ),
                    ));
                }
            }
            LaneRecord::WriteDeferred { target, .. } => {
                validate_exact_provisioned_entry(&entries_by_id, target)?;
            }
            LaneRecord::Usage { .. } => {}
            LaneRecord::OperationStarted { .. } => unreachable!("handled above"),
        }
    }
    Ok(())
}

fn by_sequence<'a, T>(values: &'a [T], seq_of: impl Fn(&T) -> u64) -> Vec<&'a T> {
    let mut v: Vec<&T> = values.iter().collect();
    v.sort_by_key(|x| seq_of(x));
    v
}

fn derive_effective_configuration(input: &LaneReductionInput) -> EffectiveLaneConfiguration {
    let mut configuration = input.defaults.clone();
    let mut entries_by_id: HashMap<String, &Entry> = HashMap::new();
    for entry in input.configuration_entries.iter().chain(input.own_entries) {
        entries_by_id.insert(entry_id(entry).to_string(), entry);
    }
    let mut ordered: Vec<&Entry> = entries_by_id.values().copied().collect();
    ordered.sort_by_key(|e| entry_seq(e));
    for entry in ordered {
        match entry {
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                configuration.provider = provider.clone();
                configuration.model_id = model_id.clone();
            }
            Entry::ThinkingLevel { thinking_level, .. } => {
                configuration.thinking_level = thinking_level.clone();
            }
            Entry::ActiveTools {
                active_tool_names, ..
            } => {
                configuration.active_tool_names = active_tool_names.clone();
            }
            Entry::Message { message, .. } => {
                if let Some(assistant) = as_assistant(message) {
                    if let Some(provider) = assistant.provider() {
                        configuration.provider = provider.to_string();
                    }
                    if let Some(model) = assistant.model() {
                        configuration.model_id = model.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    configuration
}

fn derive_newest_own(entry: Option<&Entry>) -> Option<NewestOwn> {
    let entry = entry?;
    if !matches!(entry, Entry::Message { .. }) {
        return Some(NewestOwn {
            entry_id: entry_id(entry).to_string(),
            entry_type: entry_type_name(entry).to_string(),
            role: None,
            stop_reason: None,
        });
    }
    let Entry::Message { message, .. } = entry else {
        return None;
    };
    Some(NewestOwn {
        entry_id: entry_id(entry).to_string(),
        entry_type: entry_type_name(entry).to_string(),
        role: Some(message.role().to_string()),
        stop_reason: if let Some(assistant) = as_assistant(message) {
            assistant.stop_reason()
        } else {
            None
        },
    })
}

fn entry_type_name(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message { .. } => "message",
        Entry::ModelChange { .. } => "model_change",
        Entry::ThinkingLevel { .. } => "thinking_level_change",
        Entry::ActiveTools { .. } => "active_tools_change",
        Entry::Compaction { .. } => "compaction",
        Entry::BranchSummary { .. } => "branch_summary",
        Entry::Custom { .. } => "custom",
    }
}

fn derive_tool_batch(
    operation_id: &str,
    records: &[&LaneRecord],
    own_entries: &[&Entry],
    entries_by_id: &HashMap<&str, &Entry>,
    deferred_write_ids: &HashSet<&str>,
) -> Option<ToolBatchState> {
    let assistant_entry = own_entries
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry,
                Entry::Message { message, .. }
                    if as_assistant(message)
                        .map(|a| a.content().iter().any(|c| matches!(c, ContentBlock::ToolCall { .. })))
                        .unwrap_or(false)
            )
        })
        .copied()?;
    let Entry::Message { message, .. } = assistant_entry else {
        return None;
    };
    let Some(assistant) = as_assistant(message) else {
        return None;
    };
    let tool_calls: Vec<&ContentBlock> = assistant
        .content()
        .iter()
        .filter(|c| matches!(c, ContentBlock::ToolCall { .. }))
        .collect();
    let mut starts: HashMap<u64, &LaneRecord> = HashMap::new();
    for record in records {
        if let LaneRecord::ToolStarted {
            run_id,
            assistant_entry_id: aeid,
            tool_index,
            ..
        } = record
        {
            if run_id == operation_id && aeid == entry_id(assistant_entry) {
                starts.insert(*tool_index, record);
            }
        }
    }

    let truncated = assistant.stop_reason() == Some(StopReason::Length);
    let mut calls = Vec::new();
    let mut any_unresolved = false;
    for (tool_index, tool_call) in tool_calls.iter().enumerate() {
        let tc_id = match tool_call {
            ContentBlock::ToolCall { id, .. } => id.as_str(),
            _ => continue,
        };
        let started = starts.get(&(tool_index as u64)).copied();
        let started_result = started.and_then(|s| match s {
            LaneRecord::ToolStarted {
                result_entry_id, ..
            } => entries_by_id.get(result_entry_id.as_str()).copied(),
            _ => None,
        });
        let blocked_result = own_entries.iter().find(|entry| {
            if entry_seq(entry) <= entry_seq(assistant_entry) {
                return false;
            }
            if deferred_write_ids.contains(entry_id(entry)) {
                return false;
            }
            matches!(
                entry,
                Entry::Message { message, .. }
                    if as_tool_result(message)
                        .map(|tr| tr.tool_call_id() == tc_id)
                        .unwrap_or(false)
            )
        });
        let result = started_result.or(blocked_result.map(|r| *r));
        let result_exists = result.is_some();
        let terminate = matches!(
            result,
            Some(Entry::Message {
                terminate: Some(true),
                ..
            })
        );
        if !result_exists {
            any_unresolved = true;
        }
        calls.push(ToolCallState {
            tool_index: tool_index as u64,
            tool_call: (*tool_call).clone(),
            started: started.cloned(),
            result_exists,
            terminate,
        });
    }
    Some(ToolBatchState {
        assistant_entry_id: entry_id(assistant_entry).to_string(),
        calls,
        truncated,
        unresolved: any_unresolved,
    })
}

/// Purely reconstructs one lane's orchestration state from its bounded
/// recovery inputs (upstream `reduceLaneState`).
pub fn reduce_lane_state(
    input: &LaneReductionInput,
) -> Result<LaneReductionResult, RecordLogCorruption> {
    validate_record_log(&RecordLogSlice {
        lane: input.lane,
        open_operations: input.open_operations,
        records: input.records,
        entries: input.entries,
    })?;

    let records: Vec<&LaneRecord> = by_sequence(input.records, record_seq);
    let own_entries: Vec<&Entry> = by_sequence(input.own_entries, entry_seq);
    let mut entries_by_id: HashMap<&str, &Entry> = HashMap::new();
    for entry in input.entries.iter().chain(input.own_entries.iter()) {
        entries_by_id.insert(entry_id(entry), entry);
    }
    let cancelled_queue_ids: HashSet<&str> = records
        .iter()
        .filter(|r| matches!(r, LaneRecord::QueueCancelled { .. }))
        .map(|r| match r {
            LaneRecord::QueueCancelled { entry_id, .. } => entry_id.as_str(),
            _ => unreachable!(),
        })
        .collect();
    let pending_queue_records: Vec<&&LaneRecord> = records
        .iter()
        .filter(|r| {
            matches!(r, LaneRecord::QueueEnqueued { target, .. } if !entries_by_id.contains_key(target.get("id").and_then(|v| v.as_str()).unwrap_or("")))
                && !cancelled_queue_ids.contains(target_of(r).map(|t| t.get("id").and_then(|v| v.as_str()).unwrap_or("")).unwrap_or(""))
        })
        .collect();
    let started = input.open_operations.first();
    let captured_initial_message_ids: HashSet<String> = match started {
        Some(LaneRecord::OperationStarted {
            intent: OperationIntent::Run {
                initial_messages, ..
            },
            ..
        }) => initial_messages
            .iter()
            .map(|e| entry_id_of_no_stats(e).to_string())
            .collect(),
        _ => HashSet::new(),
    };
    let pending_next_run: Vec<serde_json::Value> = pending_queue_records
        .iter()
        .filter(|r| {
            matches!(r, LaneRecord::QueueEnqueued { queue, target, .. } if queue == "nextRun"
                && target.get("id").and_then(|v| v.as_str()).map(|id| !captured_initial_message_ids.contains(id)).unwrap_or(true))
        })
        .map(|r| match r {
            LaneRecord::QueueEnqueued { target, .. } => target.clone(),
            _ => unreachable!(),
        })
        .collect();
    let effective_configuration = derive_effective_configuration(input);

    let Some(started) = started else {
        return Ok(LaneReductionResult {
            lane_state: LaneState {
                lane: input.lane.to_string(),
                leaf_id: input.leaf_id.map(|s| s.to_string()),
                operation: None,
                pending_next_run,
            },
            effective_configuration,
            terminal_failure: None,
        });
    };
    let started_id = record_id(started);
    let operation_records: Vec<&LaneRecord> = records
        .iter()
        .copied()
        .filter(|record| {
            if let LaneRecord::OperationStarted { id, .. } = record {
                id == started_id
            } else {
                record_run_id(record) == Some(started_id)
            }
        })
        .collect();
    let aborting = operation_records
        .iter()
        .any(|r| matches!(r, LaneRecord::AbortRequested { .. }));
    let pending_steer: Vec<serde_json::Value> = if aborting {
        Vec::new()
    } else {
        pending_queue_records
            .iter()
            .filter(|r| {
                matches!(r, LaneRecord::QueueEnqueued { queue, run_id, .. } if queue == "steer" && run_id == started_id)
            })
            .map(|r| target_of(r).cloned().unwrap_or(serde_json::Value::Null))
            .collect()
    };
    let pending_follow_up: Vec<serde_json::Value> = if aborting {
        Vec::new()
    } else {
        pending_queue_records
            .iter()
            .filter(|r| {
                matches!(r, LaneRecord::QueueEnqueued { queue, run_id, .. } if queue == "followUp" && run_id == started_id)
            })
            .map(|r| target_of(r).cloned().unwrap_or(serde_json::Value::Null))
            .collect()
    };
    let pending_writes: Vec<serde_json::Value> = operation_records
        .iter()
        .filter(|r| matches!(r, LaneRecord::WriteDeferred { target, .. } if !entries_by_id.contains_key(target.get("id").and_then(|v| v.as_str()).unwrap_or(""))))
        .map(|r| target_of(r).cloned().unwrap_or(serde_json::Value::Null))
        .collect();
    let missing_initial_messages: Vec<serde_json::Value> = match &started {
        LaneRecord::OperationStarted {
            intent: OperationIntent::Run {
                initial_messages, ..
            },
            ..
        } => initial_messages
            .iter()
            .filter(|e| !entries_by_id.contains_key(entry_id_of_no_stats(e)))
            .map(provisioned_value_from_entry_no_stats)
            .collect(),
        _ => Vec::new(),
    };

    let newest_attempt = operation_records
        .iter()
        .filter(|r| matches!(r, LaneRecord::StepAttempt { .. }))
        .last()
        .copied();
    let step = match newest_attempt {
        Some(LaneRecord::StepAttempt {
            step,
            attempt,
            result_entry_id,
            compaction_reason,
            ..
        }) if !entries_by_id.contains_key(result_entry_id.as_str()) => Some(StepState {
            kind: step.clone(),
            attempts: *attempt,
            result_entry_id: result_entry_id.clone(),
            compaction_reason: compaction_reason.clone(),
        }),
        _ => None,
    };

    let mut consumed_input_ids: HashSet<String> = HashSet::new();
    if let LaneRecord::OperationStarted {
        intent: OperationIntent::Run {
            initial_messages, ..
        },
        ..
    } = started
    {
        for target in initial_messages {
            consumed_input_ids.insert(entry_id_of_no_stats(target).to_string());
        }
    }
    for record in &operation_records {
        if let LaneRecord::QueueEnqueued { queue, target, .. } = record {
            if queue != "nextRun" {
                if let Some(id) = target.get("id").and_then(|v| v.as_str()) {
                    consumed_input_ids.insert(id.to_string());
                }
            }
        }
    }
    let mut newest_consumed_input_sequence: i64 = i64::MIN;
    for id in &consumed_input_ids {
        if let Some(entry) = entries_by_id.get(id.as_str()) {
            if matches!(entry, Entry::Message { .. }) {
                newest_consumed_input_sequence =
                    newest_consumed_input_sequence.max(entry_seq(entry) as i64);
            }
        }
    }
    let overflow_recovery_used = operation_records.iter().any(|record| {
        matches!(record, LaneRecord::StepAttempt { step, compaction_reason, seq, .. }
            if step == "compaction" && compaction_reason.as_deref() == Some("overflow")
                && *seq as i64 > newest_consumed_input_sequence)
    });

    let newest_own_entry = own_entries.last().copied();
    let newest_own = derive_newest_own(newest_own_entry);
    let deferred = match newest_own_entry {
        Some(Entry::Message { message, .. })
            if as_assistant(message)
                .map(|a| a.stop_reason() == Some(StopReason::Deferred))
                .unwrap_or(false) =>
        {
            as_assistant(message).and_then(|a| a.deferred().cloned())
        }
        _ => None,
    };
    let mut targets = OperationTargets::default();
    match &started {
        LaneRecord::OperationStarted {
            intent: OperationIntent::Compaction {
                result_entry_id, ..
            },
            ..
        } => {
            targets.result = entries_by_id.contains_key(result_entry_id.as_str());
        }
        LaneRecord::OperationStarted {
            intent: OperationIntent::Navigation {
                summary_entry_id, ..
            },
            ..
        } => {
            if let Some(summary_id) = summary_entry_id {
                targets.summary = entries_by_id.contains_key(summary_id.as_str());
            }
        }
        _ => {}
    }

    let deferred_write_ids: HashSet<&str> = operation_records
        .iter()
        .filter(|r| matches!(r, LaneRecord::WriteDeferred { .. }))
        .filter_map(|r| target_of(r).and_then(|t| t.get("id").and_then(|v| v.as_str())))
        .collect();

    let mut terminal_failure: Option<TerminalFailureState> = None;
    if let Some(entry @ Entry::Message { message, .. }) = newest_own_entry {
        let is_terminal_error = as_assistant(message)
            .map(|a| a.stop_reason() == Some(StopReason::Error))
            .unwrap_or(false);
        if is_terminal_error && !deferred_write_ids.contains(entry_id(entry)) {
            let assistant = as_assistant(message).expect("role checked above");
            let produced_by_step = operation_records.iter().any(|r| {
                matches!(r, LaneRecord::StepAttempt { result_entry_id, .. } if result_entry_id == entry_id(entry))
            });
            let previous_own_entry = own_entries.get(own_entries.len().wrapping_sub(2)).copied();
            let produced_by_deferred_fetch = operation_records.iter().any(|r| {
                matches!(r, LaneRecord::Usage { cause, entry_id: eid, .. } if cause == "deferred_fetch" && eid == entry_id(entry))
            }) || matches!(
                previous_own_entry,
                Some(Entry::Message { message, .. })
                    if as_assistant(message).map(|p| p.stop_reason() == Some(StopReason::Deferred)).unwrap_or(false)
            );
            // The error must be attributable to the open operation's step or a
            // deferred fetch; otherwise upstream leaves terminalFailure null.
            let source = if produced_by_step {
                Some(TerminalFailureSource::Step)
            } else if produced_by_deferred_fetch {
                Some(TerminalFailureSource::DeferredFetch)
            } else {
                None
            };
            if let Some(source) = source {
                terminal_failure = Some(TerminalFailureState {
                    entry_id: entry_id(entry).to_string(),
                    source,
                    message: assistant.clone(),
                });
            }
        }
    }

    let tool_batch = derive_tool_batch(
        started_id,
        &operation_records,
        &own_entries,
        &entries_by_id,
        &deferred_write_ids,
    );

    Ok(LaneReductionResult {
        lane_state: LaneState {
            lane: input.lane.to_string(),
            leaf_id: input.leaf_id.map(|s| s.to_string()),
            operation: Some(Box::new(LaneOperationState {
                id: started_id.to_string(),
                kind: intent_kind(&started).to_string(),
                aborting,
                step,
                tool_batch,
                missing_initial_messages,
                pending_steer,
                pending_follow_up,
                pending_writes,
                deferred,
                overflow_recovery_used,
                newest_own,
                targets,
            })),
            pending_next_run,
        },
        effective_configuration,
        terminal_failure,
    })
}

fn intent_kind(record: &LaneRecord) -> &'static str {
    match record {
        LaneRecord::OperationStarted {
            intent: OperationIntent::Run { .. },
            ..
        } => "run",
        LaneRecord::OperationStarted {
            intent: OperationIntent::Compaction { .. },
            ..
        } => "compaction",
        LaneRecord::OperationStarted {
            intent: OperationIntent::Navigation { .. },
            ..
        } => "navigation",
        _ => "unknown",
    }
}

fn target_of(record: &LaneRecord) -> Option<&serde_json::Value> {
    match record {
        LaneRecord::QueueEnqueued { target, .. } | LaneRecord::WriteDeferred { target, .. } => {
            Some(target)
        }
        _ => None,
    }
}

fn entry_id_of_no_stats(entry: &crate::session::types::EntryNoStats) -> &str {
    match entry {
        crate::session::types::EntryNoStats::Message { id, .. }
        | crate::session::types::EntryNoStats::ModelChange { id, .. }
        | crate::session::types::EntryNoStats::ThinkingLevel { id, .. }
        | crate::session::types::EntryNoStats::ActiveTools { id, .. }
        | crate::session::types::EntryNoStats::Compaction { id, .. }
        | crate::session::types::EntryNoStats::BranchSummary { id, .. }
        | crate::session::types::EntryNoStats::Custom { id, .. } => id.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{EntryNoStats, LaneRecord};
    use pi_ai::types::{ContentBlock, Message, UserContent};

    fn msg_entry(id: &str, seq: u64, message: crate::types::AgentMessage) -> Entry {
        Entry::Message {
            id: id.into(),
            seq,
            parent_id: None,
            timestamp: 1,
            message,
            terminate: None,
        }
    }

    fn assistant(id: &str, seq: u64, content: Vec<ContentBlock>) -> Entry {
        let mut a = pi_ai::types::AssistantMessage::new();
        a.set_content(content);
        a.set_stop_reason(StopReason::Stop);
        msg_entry(
            id,
            seq,
            crate::types::AgentMessage::Core(Message::Assistant(a)),
        )
    }

    fn user(id: &str, seq: u64) -> Entry {
        msg_entry(
            id,
            seq,
            crate::types::AgentMessage::Core(Message::User(UserContent::blocks(vec![], seq))),
        )
    }

    fn started(id: &str, seq: u64) -> LaneRecord {
        LaneRecord::OperationStarted {
            id: id.into(),
            seq,
            lane: "l".into(),
            timestamp: 1,
            source_leaf_id: Some("u1".into()),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![EntryNoStats::Message {
                    id: "u1".into(),
                    message: crate::types::AgentMessage::Core(Message::User(UserContent::blocks(
                        vec![],
                        2,
                    ))),
                    terminate: None,
                }],
                system_prompt_override: None,
                resume_data: None,
            },
        }
    }

    fn input<'a>(
        records: &'a [LaneRecord],
        entries: &'a [Entry],
        own: &'a [Entry],
    ) -> LaneReductionInput<'a> {
        let operations: Vec<LaneRecord> = records
            .iter()
            .filter(|r| matches!(r, LaneRecord::OperationStarted { .. }))
            .cloned()
            .collect();
        let leaked: &'a [LaneRecord] = Box::leak(operations.into_boxed_slice());
        LaneReductionInput {
            lane: "l",
            leaf_id: None,
            open_operations: leaked,
            records,
            entries,
            own_entries: own,
            configuration_entries: &[],
            defaults: EffectiveLaneConfiguration::new("anthropic", "claude", "off", vec![]),
        }
    }

    #[test]
    fn idle_lane_reduces_to_null_operation() {
        let result = reduce_lane_state(&input(&[], &[], &[])).unwrap();
        assert!(result.lane_state.operation.is_none());
        assert!(result.lane_state.pending_next_run.is_empty());
        assert_eq!(result.effective_configuration.provider, "anthropic");
        assert!(result.terminal_failure.is_none());
    }

    #[test]
    fn run_operation_with_inflight_step_reconstructs_step() {
        let start = started("op1", 1);
        let attempt = LaneRecord::StepAttempt {
            id: "a1".into(),
            seq: 3,
            lane: "l".into(),
            timestamp: 1,
            run_id: "op1".into(),
            step: "assistant".into(),
            attempt: 1,
            result_entry_id: "r1".into(),
            compaction_reason: None,
        };
        // Not yet materialized result -> step still open; the initial message
        // is not among the fetched entries -> captured as missing.
        let result = reduce_lane_state(&input(&[start, attempt], &[], &[])).unwrap();
        let op = result.lane_state.operation.expect("operation open");
        assert_eq!(op.id, "op1");
        assert_eq!(op.kind, "run");
        assert!(!op.aborting);
        let step = op.step.expect("in-flight step");
        assert_eq!(step.kind, "assistant");
        assert_eq!(step.attempts, 1);
        assert_eq!(step.result_entry_id, "r1");
        // initial message missing -> captured in missingInitialMessages
        assert_eq!(op.missing_initial_messages.len(), 1);
        assert!(result.terminal_failure.is_none());
    }

    #[test]
    fn materialized_result_closes_step_and_reports_terminal_failure_only_when_attributed() {
        let start = started("op1", 1);
        let user1 = user("u1", 2);
        let mut err_assistant = assistant("r1", 3, vec![ContentBlock::text("boom")]);
        if let Entry::Message { message, .. } = &mut err_assistant {
            match message {
                crate::types::AgentMessage::Core(Message::Assistant(a)) => {
                    a.set_stop_reason(StopReason::Error)
                }
                _ => unreachable!(),
            }
        }
        let attempt = LaneRecord::StepAttempt {
            id: "a1".into(),
            seq: 3,
            lane: "l".into(),
            timestamp: 1,
            run_id: "op1".into(),
            step: "assistant".into(),
            attempt: 1,
            result_entry_id: "r1".into(),
            compaction_reason: None,
        };
        let records = vec![start.clone(), attempt];
        let entries = vec![user1];
        let own = vec![err_assistant];
        let result = reduce_lane_state(&input(&records, &entries, &own)).unwrap();
        let op = result.lane_state.operation.expect("operation open");
        assert!(op.step.is_none(), "result materialized -> step closed");
        let tf = result
            .terminal_failure
            .expect("step-attributed error is terminal");
        assert_eq!(tf.entry_id, "r1");
        assert_eq!(tf.source, TerminalFailureSource::Step);
        assert_eq!(tf.message.stop_reason(), Some(StopReason::Error));
        // order check: assistant entry emitted as newest own
        assert_eq!(
            op.newest_own.as_ref().map(|n| n.entry_id.as_str()),
            Some("r1")
        );
    }

    #[test]
    fn duplicate_tool_invocation_is_corrupt() {
        let start = started("op1", 1);
        let user1 = user("u1", 2);
        let asst = assistant(
            "a1",
            3,
            vec![ContentBlock::tool_call("c1", "bash", serde_json::json!({}))],
        );
        let tool_start = LaneRecord::ToolStarted {
            id: "t1".into(),
            seq: 4,
            lane: "l".into(),
            timestamp: 1,
            run_id: "op1".into(),
            assistant_entry_id: "a1".into(),
            tool_index: 0,
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            effective_args: serde_json::json!({}),
            result_entry_id: "tr1".into(),
            replay: "never".into(),
        };
        let tool_start2 = LaneRecord::ToolStarted {
            id: "t2".into(),
            seq: 5,
            lane: "l".into(),
            timestamp: 1,
            run_id: "op1".into(),
            assistant_entry_id: "a1".into(),
            tool_index: 0,
            tool_call_id: "c1".into(),
            tool_name: "bash".into(),
            effective_args: serde_json::json!({}),
            result_entry_id: "tr1".into(),
            replay: "never".into(),
        };
        let records = vec![start, tool_start, tool_start2];
        let entries = vec![user1, asst];
        let err = reduce_lane_state(&input(&records, &entries, &[])).unwrap_err();
        assert_eq!(err.reason, CorruptionReason::DuplicateToolInvocation);
    }

    #[test]
    fn unknown_operation_reference_is_corrupt() {
        let record = LaneRecord::AbortRequested {
            id: "x".into(),
            seq: 1,
            lane: "l".into(),
            timestamp: 1,
            run_id: "nope".into(),
        };
        let err = validate_record_log(&RecordLogSlice {
            lane: "l",
            open_operations: &[],
            records: &[record],
            entries: &[],
        })
        .unwrap_err();
        assert_eq!(err.reason, CorruptionReason::UnknownOperation);
    }

    #[test]
    fn multiple_open_operations_is_corrupt() {
        let a = started("op1", 1);
        let b = started("op2", 2);
        let err = validate_record_log(&RecordLogSlice {
            lane: "l",
            open_operations: &[a, b],
            records: &[],
            entries: &[],
        })
        .unwrap_err();
        assert_eq!(err.reason, CorruptionReason::MultipleOpenOperations);
    }

    #[test]
    fn queue_after_abort_is_corrupt() {
        let start = started("op1", 1);
        let abort = LaneRecord::AbortRequested {
            id: "ab".into(),
            seq: 2,
            lane: "l".into(),
            timestamp: 1,
            run_id: "op1".into(),
        };
        let target = serde_json::json!({"id": "s1", "type": "message", "message": {"role": "user", "content": "x"}, "timestamp": 1});
        let enqueue = LaneRecord::QueueEnqueued {
            id: "qe".into(),
            seq: 3,
            lane: "l".into(),
            timestamp: 1,
            queue: "steer".into(),
            run_id: "op1".into(),
            target: target.clone(),
        };
        let records = vec![start, abort, enqueue];
        let err = validate_record_log(&RecordLogSlice {
            lane: "l",
            open_operations: &[],
            records: &records,
            entries: &[],
        })
        .unwrap_err();
        assert_eq!(err.reason, CorruptionReason::QueueAfterAbort);
    }
}
