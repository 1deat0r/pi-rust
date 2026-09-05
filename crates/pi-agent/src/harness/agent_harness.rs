//! Agent harness composition surface — port of
//! `packages/agent/src/harness/agent-harness.ts`.
//!
//! The upstream module defines the run loop, session, compaction, resources,
//! events, and telemetry composition surface. This port mirrors the full
//! public surface: tagged operation errors, outcome/result unions,
//! snapshot/action types, hook/event registries, and the `AgentHarness` state
//! holder with defensive-copy configuration getters and setters.
//!
//! Documented divergences:
//! - Upstream `TaggedError` subclasses (LaneBusy, MissingIdentities, ...)
//!   are flattened onto `HarnessError::Tagged(TaggedError)` with the same
//!   `_tag` strings and payload keys. `HarnessClosed` and `HarnessFault` keep
//!   their own variants; every operation implemented here returns either a
//!   real outcome or a concrete tagged/configuration error.
//! - `AgentHarnessOptions.models` (the pi-ai `Models` facade) is omitted; the
//!   harness currently reaches models through the `SimpleModels` seam
//!   (`harness/models.rs`) or explicit per-call stream functions.
//! - `toolContext` / `systemPrompt` / `toProviderMessages` accept the same
//!   logical inputs as upstream but are stored as plain values/functions at
//!   the concrete types available in the Rust port (`serde_json::Value`,
//!   `String`, message-conversion closure).
//! - `toProviderMessages` and `entryProjectors` are applied to the underlying
//!   Agent/context builder. `toolContext` has no field in the current
//!   `AgentTool` callback ABI, and `streamOptions` cannot be forwarded because
//!   `StreamFn` accepts only `(Model, Context)`; both remain available through
//!   the harness getters and are covered by boundary tests rather than being
//!   silently treated as provider request options.
//! - `drive` is retained and copied into lane views, but the lower-level
//!   `Agent` exposes no manual action reducer or drive setter. Prompt and
//!   structural methods therefore execute the real operation immediately;
//!   `peekAction`/`executeAction` observe and settle that operation instead of
//!   fabricating reducer transitions.
//! - `session` (upstream `SessionTree`) is the `Session<F>` facade; only one
//!   session reference is stored (upstream aliases `durableSession`/`session`).
//! - The pinned upstream oracle does not yet contain the durable run-loop
//!   implementation described by its design document. Rust keeps the same
//!   operation/result shapes but executes against `Session` and the real
//!   `Agent`; provider-backed summary generation requires a configured stream
//!   function.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use pi_ai::model::Model;
use pi_ai::types::{
    AssistantMessage, ContentBlock, DeferredHandle, Message, SimpleStreamOptions, Usage,
    UserContent,
};
use pi_ai::utils::retry::RetryPolicy;
use pi_telemetry::{
    InMemoryTelemetryContext, SpanError, SpanHandle, SpanOptions, SpanStatus, TelemetryContext,
    TelemetrySpan,
};

use crate::fs::FileSystem;
use crate::harness::compaction::compaction::{CompactionSettings, DEFAULT_COMPACTION_SETTINGS};
use crate::harness::events::{
    HarnessEvent, HarnessEventBus, HarnessEventListener, RunEndEvent, RunOutcome as EventOutcome,
    RunStartEvent,
};
use crate::harness::models::{BoxFuture, SimpleModels};
use crate::harness::result::TaggedError;
use crate::rich_agent::{AfterToolCallHook, Agent, BeforeToolCallHook, OverflowRecoveryHook};
use crate::session::session::Session;
use crate::session::state::{BranchBounds, EntryOrder, EntryQuery, RecordQuery};
use crate::session::types::{Entry, EntryNoStats, LaneRecord, NewRecord, OperationIntent};
use crate::tools::{AgentTool, ToolExecuteFn, ToolPrepareArgumentsFn};
use crate::types::{AgentHarnessResources, AgentMessage, SessionError};
use pi_ai::types::{ModelThinkingLevel, Tool};

// ---------------------------------------------------------------------------
// Telemetry context holder (see module docs)
// ---------------------------------------------------------------------------

/// The concrete telemetry contexts currently available in the Rust port.
/// Upstream accepts any `TelemetryContext`; the pi-telemetry trait is not
/// object-safe, so harness options carry this small dispatch enum.
#[derive(Debug, Clone, Default)]
pub enum HarnessTelemetryContext {
    #[default]
    Noop,
    InMemory(Arc<InMemoryTelemetryContext>),
}

impl HarnessTelemetryContext {
    pub fn noop() -> Self {
        HarnessTelemetryContext::Noop
    }

    /// Return the provider-facing telemetry context when this harness owns a
    /// concrete recorder. A noop harness must not manufacture a recorder and
    /// therefore returns `None`.
    pub fn provider_context(&self) -> Option<InMemoryTelemetryContext> {
        match self {
            HarnessTelemetryContext::Noop => None,
            HarnessTelemetryContext::InMemory(context) => Some((**context).clone()),
        }
    }

    pub fn start_span<T>(
        &self,
        options: SpanOptions,
        callback: impl FnOnce(&SpanHandle) -> T,
    ) -> T {
        match self {
            HarnessTelemetryContext::Noop => {
                pi_telemetry::NOOP_TELEMETRY_CONTEXT.start_span(options, callback)
            }
            HarnessTelemetryContext::InMemory(ctx) => ctx.start_span(options, callback),
        }
    }

    pub async fn start_span_async<T, F, Fut>(&self, options: SpanOptions, callback: F) -> T
    where
        F: FnOnce(SpanHandle) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        match self {
            HarnessTelemetryContext::Noop => {
                pi_telemetry::NOOP_TELEMETRY_CONTEXT
                    .start_span_async(options, callback)
                    .await
            }
            HarnessTelemetryContext::InMemory(ctx) => ctx.start_span_async(options, callback).await,
        }
    }
}

/// Run a mode-owned loop under the harness run lifecycle.
///
/// Modes that still own their historical context/session plumbing can use
/// this bridge without changing their existing wire or TUI events. The
/// callback receives the live run span so mode-specific events can be nested
/// under the same operation when needed.
pub async fn run_with_harness_lifecycle<T, F, Fut>(
    telemetry: &HarnessTelemetryContext,
    event_bus: &mut HarnessEventBus,
    lane: &str,
    session_id: &str,
    leaf_id: String,
    callback: F,
) -> Result<T, HarnessError>
where
    F: FnOnce(SpanHandle) -> Fut,
    Fut: std::future::Future<Output = Result<T, HarnessError>>,
{
    let run_id = crate::session::new_id();
    event_bus.emit(&HarnessEvent::RunStart(RunStartEvent {
        lane: lane.to_string(),
        run_id: run_id.clone(),
    }));

    let span_run_id = run_id.clone();
    let span_lane = lane.to_string();
    let span_session_id = session_id.to_string();
    let result = telemetry
        .start_span_async(
            SpanOptions {
                name: "pi.harness.run".to_string(),
                attributes: Some(BTreeMap::from([
                    (
                        "pi.session.id".to_string(),
                        serde_json::json!(span_session_id),
                    ),
                    ("pi.lane.name".to_string(), serde_json::json!(span_lane)),
                    (
                        "pi.operation.id".to_string(),
                        serde_json::json!(span_run_id),
                    ),
                    (
                        "pi.operation.recovery".to_string(),
                        serde_json::json!(false),
                    ),
                    ("pi.operation.kind".to_string(), serde_json::json!("run")),
                ])),
            },
            move |span| async move {
                span.add_event("run_start", None);
                let result = callback(span.clone()).await;
                let outcome = if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                };
                if let Err(error) = &result {
                    span.set_status(SpanStatus::Error {
                        error: Some(SpanError {
                            name: "HarnessError".to_string(),
                            message: error.to_string(),
                        }),
                    });
                }
                span.set_attributes(BTreeMap::from([(
                    "pi.operation.outcome".to_string(),
                    serde_json::json!(outcome),
                )]));
                span.add_event(
                    "run_end",
                    Some(BTreeMap::from([(
                        "pi.operation.outcome".to_string(),
                        serde_json::json!(outcome),
                    )])),
                );
                result
            },
        )
        .await;

    event_bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
        lane: lane.to_string(),
        run_id,
        outcome: if result.is_ok() {
            EventOutcome::Completed
        } else {
            EventOutcome::Failed
        },
        leaf_id,
    }));
    result
}

// ---------------------------------------------------------------------------
// Error surface
// ---------------------------------------------------------------------------

/// Stable operation error (upstream `OperationError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

/// The rejected error union for harness operations.
///
/// `Tagged` carries the stable tagged-error classes (LaneBusy,
/// MissingIdentities, ...), `Closed` mirrors `HarnessClosed`, and `Fault`
/// mirrors `HarnessFault` (an untagged wrapper around an underlying cause).
#[derive(Debug, Clone)]
pub enum HarnessError {
    Tagged(TaggedError),
    Closed,
    Fault { message: String },
}

impl HarnessError {
    pub fn tagged(tag: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::Tagged(TaggedError::new(tag, message))
    }
    pub fn closed() -> Self {
        HarnessError::Closed
    }
    pub fn fault(message: impl Into<String>) -> Self {
        HarnessError::Fault {
            message: message.into(),
        }
    }

    /// Convenience constructors matching the upstream TaggedError classes.
    pub fn lane_busy(lane: &str, operation_id: &str, operation_kind: &str) -> Self {
        HarnessError::tagged("LaneBusy", format!("lane {lane} is busy"))
            .with_payload("lane", lane.to_string())
            .with_payload("operationId", operation_id.to_string())
            .with_payload("operationKind", operation_kind.to_string())
    }
    pub fn missing_identities(lane: &str, tools: Vec<String>, models: Vec<String>) -> Self {
        HarnessError::tagged(
            "MissingIdentities",
            format!("lane {lane} is missing tools/models"),
        )
        .with_payload("lane", lane.to_string())
        .with_payload("tools", serde_json::to_value(tools).unwrap_or_default())
        .with_payload("models", serde_json::to_value(models).unwrap_or_default())
    }
    pub fn no_active_run(lane: &str) -> Self {
        HarnessError::tagged("NoActiveRun", format!("no active run in lane {lane}"))
            .with_payload("lane", lane.to_string())
    }
    pub fn no_active_operation(lane: &str) -> Self {
        HarnessError::tagged(
            "NoActiveOperation",
            format!("no active operation in lane {lane}"),
        )
        .with_payload("lane", lane.to_string())
    }
    pub fn nothing_to_resume(lane: &str) -> Self {
        HarnessError::tagged(
            "NothingToResume",
            format!("nothing to resume in lane {lane}"),
        )
        .with_payload("lane", lane.to_string())
    }
    pub fn invalid_message(lane: &str, reason: &str) -> Self {
        HarnessError::tagged(
            "InvalidMessage",
            format!("invalid message for lane {lane}: {reason}"),
        )
        .with_payload("lane", lane.to_string())
        .with_payload("reason", reason.to_string())
    }
    pub fn unknown_skill(name: &str) -> Self {
        HarnessError::tagged("UnknownSkill", format!("unknown skill {name}"))
            .with_payload("name", name.to_string())
    }
    pub fn unknown_template(name: &str) -> Self {
        HarnessError::tagged("UnknownTemplate", format!("unknown prompt template {name}"))
            .with_payload("name", name.to_string())
    }
    pub fn unknown_target(target_id: &str) -> Self {
        HarnessError::tagged("UnknownTarget", format!("unknown target {target_id}"))
            .with_payload("targetId", target_id.to_string())
    }
    pub fn invalid_navigation(lane: &str, reason: &str) -> Self {
        HarnessError::tagged(
            "InvalidNavigation",
            format!("invalid navigation in lane {lane}: {reason}"),
        )
        .with_payload("lane", lane.to_string())
        .with_payload("reason", reason.to_string())
    }
    pub fn unknown_queue_item(lane: &str, entry_id: &str) -> Self {
        HarnessError::tagged(
            "UnknownQueueItem",
            format!("unknown queued item {entry_id} in lane {lane}"),
        )
        .with_payload("lane", lane.to_string())
        .with_payload("entryId", entry_id)
    }
    pub fn lane_exists(lane: &str) -> Self {
        HarnessError::tagged("LaneExists", format!("lane {lane} exists"))
            .with_payload("lane", lane.to_string())
    }
    pub fn invalid_lane(lane: &str, reason: &str) -> Self {
        HarnessError::tagged("InvalidLane", format!("invalid lane {lane}: {reason}"))
            .with_payload("lane", lane.to_string())
            .with_payload("reason", reason.to_string())
    }
    pub fn nothing_to_compact(lane: &str) -> Self {
        HarnessError::tagged(
            "NothingToCompact",
            format!("nothing to compact in lane {lane}"),
        )
        .with_payload("lane", lane.to_string())
    }

    fn with_payload(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        if let HarnessError::Tagged(tagged) = &mut self {
            tagged.payload.insert(key.to_string(), value.into());
        }
        self
    }
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HarnessError::Tagged(t) => write!(f, "{t}"),
            HarnessError::Closed => {
                write!(f, "AgentHarness was closed while the operation was active")
            }
            HarnessError::Fault { message } => write!(f, "HarnessFault: {message}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<SessionError> for HarnessError {
    fn from(error: SessionError) -> Self {
        HarnessError::fault(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// Outcome / result unions
// ---------------------------------------------------------------------------

/// Run invocation outcome (upstream `RunOutcome`).
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed {
        leaf_id: String,
        final_entry_id: String,
        final_message: AssistantMessage,
    },
    Aborted {
        leaf_id: String,
        final_entry_id: String,
        final_message: AssistantMessage,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
        final_entry_id: Option<String>,
        final_message: Option<AssistantMessage>,
    },
    Suspended {
        leaf_id: String,
        final_entry_id: String,
        deferred: DeferredHandle,
    },
}

/// Compaction invocation outcome (upstream `CompactionOutcome`).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // 1:1 upstream union; boxing would diverge
pub enum CompactionOutcome {
    Completed {
        leaf_id: String,
        entry: Entry,
    },
    Declined {
        leaf_id: String,
    },
    Aborted {
        leaf_id: String,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
    },
}

/// Navigation invocation outcome (upstream `NavigationOutcome`).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // 1:1 upstream union; boxing would diverge
pub enum NavigationOutcome {
    Completed {
        new_leaf_id: Option<String>,
        summary_entry: Option<Entry>,
    },
    Declined {
        leaf_id: Option<String>,
    },
    Aborted {
        leaf_id: Option<String>,
    },
    Failed {
        leaf_id: Option<String>,
        error: OperationError,
    },
}

/// Resume outcome tagging the recovered operation kind (upstream
/// `ResumeOutcome`).
#[derive(Debug, Clone)]
pub enum ResumeOutcome {
    Run {
        run_id: String,
        outcome: RunOutcome,
    },
    Compaction {
        run_id: String,
        outcome: CompactionOutcome,
    },
    Navigation {
        run_id: String,
        outcome: NavigationOutcome,
    },
}

#[derive(Debug, Clone)]
pub struct RunResultValue {
    pub run_id: String,
    pub outcome: RunOutcome,
}

#[derive(Debug, Clone)]
pub struct CompactionResultValue {
    pub run_id: String,
    pub outcome: CompactionOutcome,
}

#[derive(Debug, Clone)]
pub struct NavigationResultValue {
    pub run_id: String,
    pub outcome: NavigationOutcome,
}

#[derive(Debug, Clone)]
pub struct AbortResultValue {
    pub run_id: String,
    pub steer: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelQueuedOutcome {
    Cancelled,
    AlreadyConsumed,
    AlreadyCleared,
}

/// Run result: `{ runId } & RunOutcome` (upstream `RunResult`).
pub type RunResult = std::result::Result<RunResultValue, HarnessError>;
pub type CompactionResult = std::result::Result<CompactionResultValue, HarnessError>;
pub type NavigationResult = std::result::Result<NavigationResultValue, HarnessError>;
/// Queue result: `{ entryId }` (upstream `QueueResult`).
pub type QueueResult = std::result::Result<String, HarnessError>;
pub type CancelQueuedResult = std::result::Result<CancelQueuedOutcome, HarnessError>;
pub type RecordUsageResult = std::result::Result<(), HarnessError>;
pub type AbortResult = std::result::Result<AbortResultValue, HarnessError>;
pub type ResumeResult = std::result::Result<ResumeOutcome, HarnessError>;
pub type CreateLaneResult = std::result::Result<Box<dyn AgentLane>, HarnessError>;

// ---------------------------------------------------------------------------
// Operation / snapshot types
// ---------------------------------------------------------------------------

/// Operation kinds (upstream `SuspendedOperation["kind"]`, `LaneInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Run,
    Compaction,
    Navigation,
}

impl OperationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OperationKind::Run => "run",
            OperationKind::Compaction => "compaction",
            OperationKind::Navigation => "navigation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Running,
    Suspended,
    Aborting,
}

impl OperationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OperationStatus::Running => "running",
            OperationStatus::Suspended => "suspended",
            OperationStatus::Aborting => "aborting",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OperationInfo {
    pub id: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
}

/// Options for `navigate_tree` (upstream `NavigateOptions`).
#[derive(Debug, Clone, Default)]
pub struct NavigateOptions {
    pub summarize: bool,
    pub custom_instructions: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspensionReason {
    Crash,
    Deferred,
}

impl SuspensionReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SuspensionReason::Crash => "crash",
            SuspensionReason::Deferred => "deferred",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AbortingPlan {
    pub steer: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}

/// A suspended operation resumed by `resume()` (upstream
/// `SuspendedOperation`).
#[derive(Debug, Clone)]
pub struct SuspendedOperation {
    pub lane: String,
    pub kind: OperationKind,
    pub id: String,
    pub started_at: u64,
    pub reason: SuspensionReason,
    pub prompt: Option<Vec<AgentMessage>>,
    pub deferred: Option<DeferredHandle>,
    pub aborting: Option<AbortingPlan>,
    pub missing: MissingIdentitiesInfo,
}

#[derive(Debug, Clone, Default)]
pub struct MissingIdentitiesInfo {
    pub tools: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LaneInfo {
    pub name: String,
    pub leaf_id: Option<String>,
    pub operation: Option<OperationInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueuedItem {
    pub entry_id: String,
    pub message: AgentMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueItemStatus {
    Pending,
    Consumed,
    Cleared,
}

#[derive(Debug, Clone, Default)]
pub struct LaneQueues {
    pub steer: Vec<QueuedItem>,
    pub follow_up: Vec<QueuedItem>,
    pub next_run: Vec<QueuedItem>,
}

#[derive(Debug, Clone)]
pub struct PendingWrite {
    pub id: String,
    pub entry: EntryNoStats,
}

/// Point-in-time lane snapshot (upstream `LaneSnapshot`).
#[derive(Debug, Clone)]
pub struct LaneSnapshot {
    pub lane: String,
    pub transcript: Vec<Entry>,
    pub leaf_id: Option<String>,
    pub operation: Option<OperationInfo>,
    pub queues: LaneQueues,
    pub pending_writes: Vec<PendingWrite>,
    pub faulted: bool,
}

#[derive(Debug, Clone)]
pub struct LaneInfoWithSuspended {
    pub info: LaneInfo,
    pub suspended: Option<SuspendedOperation>,
}

/// Point-in-time session snapshot (upstream `SessionSnapshot`).
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub lanes: Vec<LaneInfoWithSuspended>,
    pub faulted: bool,
}

/// The next action a manual driver should execute (upstream `ActionInfo`).
#[derive(Debug, Clone)]
pub enum ActionInfo {
    AppendEntry {
        entry_type: String,
        entry_id: String,
    },
    AppendRecord {
        record_type: String,
    },
    MoveLane {
        to: Option<String>,
    },
    SetFact {
        fact: String,
    },
    TryFinishRun {
        outcome: String,
    },
    FinishOperation {
        outcome: String,
    },
    CommitFollowUp,
    ConsumeQueueItem {
        queue: String,
        entry_id: String,
    },
    ApplyPendingWrite {
        entry_id: String,
    },
    StreamAssistant {
        step: String,
        attempt: u32,
    },
    ExecuteTool {
        tool_call_id: String,
        tool_name: String,
    },
    FetchDeferred {
        provider: String,
        id: String,
    },
    CancelDeferred {
        provider: String,
        id: String,
    },
    Hook {
        name: String,
    },
    Sleep {
        delay_ms: u64,
    },
}

/// Hook names (upstream `HookName`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookName {
    BeforeRun,
    BeforeResume,
    BeforeRunEnd,
    TransformContext,
    BeforeRequest,
    BeforePayload,
    AfterResponse,
    BeforeTool,
    AfterTool,
    BeforeCompaction,
    BeforeNavigation,
}

impl HookName {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookName::BeforeRun => "before_run",
            HookName::BeforeResume => "before_resume",
            HookName::BeforeRunEnd => "before_run_end",
            HookName::TransformContext => "transform_context",
            HookName::BeforeRequest => "before_request",
            HookName::BeforePayload => "before_payload",
            HookName::AfterResponse => "after_response",
            HookName::BeforeTool => "before_tool",
            HookName::AfterTool => "after_tool",
            HookName::BeforeCompaction => "before_compaction",
            HookName::BeforeNavigation => "before_navigation",
        }
    }
}

// ---------------------------------------------------------------------------
// Tool / resources / config types
// ---------------------------------------------------------------------------

/// Replay policy declared by a harness tool (upstream
/// `HarnessTool["replay"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPolicy {
    Never,
    Safe,
}

impl ReplayPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReplayPolicy::Never => "never",
            ReplayPolicy::Safe => "safe",
        }
    }
}

/// A tool registered with the harness (upstream `HarnessTool`).
#[derive(Clone)]
pub struct HarnessTool {
    pub tool: Tool,
    pub execute: ToolExecuteFn,
    pub prepare_arguments: Option<ToolPrepareArgumentsFn>,
    pub execution_mode: Option<crate::tools::ToolExecutionMode>,
    pub replay: Option<ReplayPolicy>,
}

impl std::fmt::Debug for HarnessTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessTool")
            .field("name", &self.tool.name)
            .field("execution_mode", &self.execution_mode)
            .field("replay", &self.replay)
            .finish_non_exhaustive()
    }
}

impl HarnessTool {
    pub fn name(&self) -> &str {
        &self.tool.name
    }

    /// Adapt a registered `AgentTool` into the harness-facing tool shape
    /// without dropping argument preparation semantics.
    pub fn from_agent_tool(tool: &AgentTool) -> Self {
        Self {
            tool: tool.tool.clone(),
            execute: tool.execute.clone(),
            prepare_arguments: tool.prepare_arguments.clone(),
            execution_mode: tool.execution_mode,
            replay: None,
        }
    }

    fn to_agent_tool(&self) -> AgentTool {
        let mut tool = AgentTool::new(self.tool.clone(), self.name(), self.execute.clone());
        if let Some(prepare_arguments) = &self.prepare_arguments {
            tool = tool.with_prepare_arguments(prepare_arguments.clone());
        }
        if let Some(execution_mode) = self.execution_mode {
            tool = tool.with_execution_mode(execution_mode);
        }
        tool
    }
}

/// Resources made available to explicit invocation methods
/// (upstream `Resources`).
pub type Resources = AgentHarnessResources;

/// Queue drain mode (upstream `QueueMode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

impl QueueMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueMode::All => "all",
            QueueMode::OneAtATime => "one-at-a-time",
        }
    }
}

/// Tool batch execution mode (upstream `toolExecution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecution {
    Sequential,
    Parallel,
}

impl ToolExecution {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolExecution::Sequential => "sequential",
            ToolExecution::Parallel => "parallel",
        }
    }
}

/// Driver mode (upstream `drive`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Drive {
    #[default]
    Automatic,
    Manual,
}

/// Image content accepted by `prompt`/`steer`/`followUp`/`nextRun`
/// (upstream `ImageContent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

/// Options for `record_usage` (upstream inline `{ entryId?, details? }`).
#[derive(Debug, Clone, Default)]
pub struct RecordUsageOptions {
    pub entry_id: Option<String>,
    pub details: Option<serde_json::Value>,
}

/// Stream options (upstream `StreamOptions`).
pub type StreamOptions = SimpleStreamOptions;

/// Per-request stream option patch (upstream `StreamOptionsPatch`). `None`
/// fields are unset; header/metadata patches are keyed maps.
#[derive(Debug, Clone, Default)]
pub struct StreamOptionsPatch {
    pub transport: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub metadata: Option<BTreeMap<String, Option<serde_json::Value>>>,
    pub cache_retention: Option<serde_json::Value>,
}

/// Projects a session entry into agent messages for the model context
/// (upstream `EntryProjector`).
pub type EntryProjector = Arc<dyn Fn(&Entry) -> Vec<AgentMessage> + Send + Sync>;
/// Converts agent messages to provider `Message`s (upstream
/// `toProviderMessages`).
pub type ProviderMessageConverter =
    Arc<dyn Fn(&[AgentMessage]) -> Vec<pi_ai::types::Message> + Send + Sync>;

/// Event handler callback used by the harness events surface.
pub type EventHandler = Arc<dyn Fn(&serde_json::Value) + Send + Sync>;
/// Unsubscribe function returned by `on` registrations.
pub type UnsubscribeFn = Box<dyn FnOnce() + Send>;

/// A handle that batches snapshots until `start` (upstream `WatchHandle`).
pub struct WatchHandle<TSnapshot> {
    pub snapshot: TSnapshot,
    #[allow(dead_code)]
    pub unsubscribe: Option<Box<dyn FnOnce() + Send>>,
}

impl<TSnapshot> WatchHandle<TSnapshot> {
    pub fn new(snapshot: TSnapshot, unsubscribe: Option<Box<dyn FnOnce() + Send>>) -> Self {
        Self {
            snapshot,
            unsubscribe,
        }
    }
}

/// Callback passed to `run_when_idle` (upstream `() => void | Promise<void>`).
pub type RunWhenIdleCallback = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Options for `AgentHarness::create` (upstream `AgentHarnessOptions`).
///
/// `tool_context`/`system_prompt`/`to_provider_messages`/`entry_projectors`
/// are applied at the points supported by the Rust agent/session APIs. Fields
/// whose lower-level ABI has no corresponding input remain observable through
/// their getters and are documented as explicit boundaries in this module.
pub struct AgentHarnessOptions<F: FileSystem> {
    pub session: Session<F>,
    pub model: Model,
    pub stream_fn: Option<crate::agent::StreamFn>,
    /// Option-aware provider stream. When present it receives the effective
    /// per-request options, including telemetry and cancellation.
    pub stream_fn_with_options: Option<crate::agent::StreamFnWithOptions>,
    /// Real session-backed overflow recovery callback. The callback owns
    /// durable failed-response/compaction persistence and returns the rebuilt
    /// provider-facing context to the rich agent loop.
    pub overflow_recovery: Option<OverflowRecoveryHook>,
    /// Mutates or rejects a tool call before execution.
    pub before_tool_call: Option<BeforeToolCallHook>,
    /// Replaces or annotates a tool result after execution.
    pub after_tool_call: Option<AfterToolCallHook>,
    pub system_prompt: Option<String>,
    pub block_images: bool,
    pub tool_result_image_options: Option<crate::tools::image::ProcessImageOptions>,
    pub thinking_level: Option<ModelThinkingLevel>,
    pub active_tool_names: Option<Vec<String>>,
    pub tools: Option<Vec<HarnessTool>>,
    pub tool_context: Option<serde_json::Value>,
    pub resources: Option<Resources>,
    pub stream_options: Option<StreamOptions>,
    pub retry: Option<RetryPolicy>,
    pub compaction: Option<CompactionSettings>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub tool_execution: Option<ToolExecution>,
    pub drive: Option<Drive>,
    pub to_provider_messages: Option<ProviderMessageConverter>,
    pub entry_projectors: Option<BTreeMap<String, EntryProjector>>,
    pub context: Option<HarnessTelemetryContext>,
}

impl<F: FileSystem> AgentHarnessOptions<F> {
    pub fn new(session: Session<F>, model: Model) -> Self {
        Self {
            session,
            model,
            stream_fn: None,
            stream_fn_with_options: None,
            overflow_recovery: None,
            before_tool_call: None,
            after_tool_call: None,
            system_prompt: None,
            block_images: false,
            tool_result_image_options: None,
            thinking_level: None,
            active_tool_names: None,
            tools: None,
            tool_context: None,
            resources: None,
            stream_options: None,
            retry: None,
            compaction: None,
            steering_mode: None,
            follow_up_mode: None,
            tool_execution: None,
            drive: None,
            to_provider_messages: None,
            entry_projectors: None,
            context: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Hooks / events registry
// ---------------------------------------------------------------------------

struct RegistryEntry {
    id: usize,
    name: String,
    handler: EventHandler,
}

#[derive(Default)]
struct RegistryState {
    next_id: usize,
    entries: Vec<RegistryEntry>,
}

/// Shared synchronous hook/event registry.
///
/// The pinned TypeScript oracle exposes async-capable callbacks, while the
/// Rust public callback type is intentionally synchronous (`EventHandler`).
/// Registration, filtering, unsubscription, close behavior, and callback
/// delivery are nevertheless real and shared by all lane views. The harness
/// invokes hooks at its supported interception points and publishes passive
/// lifecycle events through the same registry.
#[derive(Clone)]
pub struct HookEventRegistry {
    closed: Arc<RwLock<bool>>,
    state: Arc<Mutex<RegistryState>>,
}

impl HookEventRegistry {
    fn new(closed: Arc<RwLock<bool>>) -> Self {
        Self {
            closed,
            state: Arc::new(Mutex::new(RegistryState::default())),
        }
    }

    fn shared_with(&self) -> Self {
        Self {
            closed: self.closed.clone(),
            state: self.state.clone(),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.read().map(|b| *b).unwrap_or(false)
    }

    /// Register a callback for one hook/event name.
    pub fn on(&self, name: &str, handler: EventHandler) -> Result<UnsubscribeFn, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let id = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let id = state.next_id;
            state.next_id = state.next_id.saturating_add(1);
            state.entries.push(RegistryEntry {
                id,
                name: name.to_string(),
                handler,
            });
            id
        };
        let state = Arc::downgrade(&self.state);
        Ok(Box::new(move || {
            if let Some(state) = state.upgrade() {
                state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .entries
                    .retain(|entry| entry.id != id);
            }
        }))
    }

    /// Deliver a payload to current listeners for exactly one name.
    fn emit(&self, name: &str, payload: &serde_json::Value) {
        let handlers: Vec<EventHandler> = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries
            .iter()
            .filter(|entry| entry.name == name)
            .map(|entry| entry.handler.clone())
            .collect();
        for handler in handlers {
            handler(payload);
        }
    }

    /// Number of registered handlers for a name. Useful to verify that
    /// unsubscribe closures remove only their own registration.
    pub fn listener_count(&self, name: &str) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries
            .iter()
            .filter(|entry| entry.name == name)
            .count()
    }
}

// ---------------------------------------------------------------------------
// AgentLane trait
// ---------------------------------------------------------------------------

/// The agent-lane surface exposed by the harness (upstream `AgentLane`).
#[async_trait]
pub trait AgentLane: Send + Sync {
    /// Stable lane name (upstream `readonly name`).
    fn lane_name(&self) -> &str;

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError>;
    async fn prompt_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<RunResultValue, HarnessError>;
    async fn prompt_messages(
        &self,
        messages: &[AgentMessage],
    ) -> Result<RunResultValue, HarnessError>;
    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> Result<RunResultValue, HarnessError>;
    async fn prompt_from_template(
        &self,
        name: &str,
        args: Option<&[String]>,
    ) -> Result<RunResultValue, HarnessError>;
    async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResultValue, HarnessError>;
    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        options: Option<&NavigateOptions>,
    ) -> Result<NavigationResultValue, HarnessError>;
    async fn resume(&self) -> Result<ResumeOutcome, HarnessError>;
    async fn abort(&self) -> Result<AbortResultValue, HarnessError>;
    async fn steer_text(&self, text: &str, images: &[ImageContent])
        -> Result<String, HarnessError>;
    async fn steer_message(&self, message: &AgentMessage) -> Result<String, HarnessError>;
    async fn follow_up_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<String, HarnessError>;
    async fn follow_up_message(&self, message: &AgentMessage) -> Result<String, HarnessError>;
    async fn next_run_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<String, HarnessError>;
    async fn next_run_message(&self, message: &AgentMessage) -> Result<String, HarnessError>;
    async fn cancel_queued(&self, entry_id: &str) -> Result<CancelQueuedOutcome, HarnessError>;
    async fn record_usage(
        &self,
        usage: &Usage,
        options: Option<&RecordUsageOptions>,
    ) -> Result<(), HarnessError>;
    async fn wait_for_idle(&self) -> Result<(), HarnessError>;
    async fn run_when_idle(&self, callback: RunWhenIdleCallback) -> Result<(), HarnessError>;
    async fn peek_action(&self) -> Result<Option<ActionInfo>, HarnessError>;
    async fn execute_action(&self) -> Result<Option<ActionInfo>, HarnessError>;
    async fn run_to_completion(&self) -> Result<(), HarnessError>;
    async fn get_model(&self) -> Model;
    async fn set_model(&mut self, model: Model);
    async fn get_thinking_level(&self) -> ModelThinkingLevel;
    async fn set_thinking_level(&mut self, level: ModelThinkingLevel);
    async fn get_active_tools(&self) -> Vec<String>;
    async fn set_active_tools(&mut self, names: Vec<String>);
    async fn get_tools(&self) -> Vec<HarnessTool>;
    async fn set_tools(&mut self, tools: Vec<HarnessTool>, active_names: Option<Vec<String>>);
    async fn get_resources(&self) -> Resources;
    async fn set_resources(&mut self, resources: Resources);
    async fn get_stream_options(&self) -> StreamOptions;
    async fn set_stream_options(&mut self, options: StreamOptions);
    async fn get_retry_policy(&self) -> RetryPolicy;
    async fn set_retry_policy(&mut self, policy: RetryPolicy);
    async fn get_compaction_settings(&self) -> CompactionSettings;
    async fn set_compaction_settings(&mut self, settings: CompactionSettings);
    async fn get_steering_mode(&self) -> QueueMode;
    async fn set_steering_mode(&mut self, mode: QueueMode);
    async fn get_follow_up_mode(&self) -> QueueMode;
    async fn set_follow_up_mode(&mut self, mode: QueueMode);
    async fn watch(&self) -> Result<WatchHandle<LaneSnapshot>, HarnessError>;
    async fn watch_session(&self) -> Result<WatchHandle<SessionSnapshot>, HarnessError>;
    async fn close(&mut self);
}

// ---------------------------------------------------------------------------
// AgentHarness
// ---------------------------------------------------------------------------

/// Agent harness state holder and composition surface (upstream
/// `AgentHarness`). Existing session records are restored into the live
/// context and open operation records are returned as suspended work.
pub struct AgentHarness<F: FileSystem> {
    name: String,
    session: Arc<AsyncMutex<Session<F>>>,
    model: Model,
    thinking_level: ModelThinkingLevel,
    active_tool_names: Vec<String>,
    tools: Vec<HarnessTool>,
    resources: Resources,
    stream_options: StreamOptions,
    retry_policy: RetryPolicy,
    compaction_settings: CompactionSettings,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    drive: Drive,
    agent: Option<Arc<Agent>>,
    stream_fn: Option<crate::agent::StreamFn>,
    stream_fn_with_options: Option<crate::agent::StreamFnWithOptions>,
    overflow_recovery: Option<OverflowRecoveryHook>,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
    /// Messages persisted by the built-in overflow callback during the
    /// current run. The normal run owner consumes these occurrences instead
    /// of appending them a second time after the rich loop returns.
    automatic_overflow_persisted: Option<Arc<Mutex<Vec<AgentMessage>>>>,
    automatic_overflow_recovery: bool,
    system_prompt: String,
    block_images: bool,
    tool_result_image_options: Option<crate::tools::image::ProcessImageOptions>,
    tool_execution: Option<ToolExecution>,
    telemetry_context: HarnessTelemetryContext,
    tool_context: Option<serde_json::Value>,
    entry_projectors: BTreeMap<String, EntryProjector>,
    to_provider_messages: Option<ProviderMessageConverter>,
    event_bus: Arc<std::sync::Mutex<HarnessEventBus>>,
    queue_state: Arc<Mutex<BTreeMap<String, LaneQueues>>>,
    queue_item_status: Arc<Mutex<BTreeMap<String, QueueItemStatus>>>,
    active_operations: Arc<Mutex<BTreeMap<String, OperationInfo>>>,
    operation_signals: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
    pending_actions: Arc<Mutex<BTreeMap<String, VecDeque<ActionInfo>>>>,
    pub hooks: HookEventRegistry,
    pub events: HookEventRegistry,
    closed: Arc<RwLock<bool>>,
}

struct HarnessAgentConfig<'a> {
    stream_fn: Option<crate::agent::StreamFn>,
    stream_fn_with_options: Option<crate::agent::StreamFnWithOptions>,
    overflow_recovery: Option<OverflowRecoveryHook>,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
    model: &'a Model,
    system_prompt: &'a str,
    tools: &'a [HarnessTool],
    active_tool_names: &'a [String],
    thinking_level: ModelThinkingLevel,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    to_provider_messages: Option<ProviderMessageConverter>,
    block_images: bool,
    tool_result_image_options: Option<crate::tools::image::ProcessImageOptions>,
    tool_execution: Option<ToolExecution>,
    stream_options: &'a StreamOptions,
    retry_policy: &'a RetryPolicy,
    telemetry_context: &'a HarnessTelemetryContext,
}

type AutomaticOverflowPersistence = Arc<Mutex<Vec<AgentMessage>>>;

fn context_with_projectors(
    entries: &[Entry],
    projectors: &BTreeMap<String, EntryProjector>,
) -> crate::session::context::SessionContext {
    let mut options = crate::session::context::SessionContextBuildOptions::default();
    for (custom_type, projector) in projectors {
        let projector = projector.clone();
        options.entry_projectors.insert(
            custom_type.clone(),
            Box::new(move |entry, _index, _entries| Some(projector(entry))),
        );
    }
    crate::session::context::build_session_context(entries, &options)
}

async fn lane_entries<F: FileSystem>(
    session: &mut Session<F>,
    lane: &str,
) -> Result<Vec<Entry>, SessionError> {
    let query = EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    if lane == "main" {
        session.find_entries(&query).await
    } else {
        session
            .view(lane)
            .find_entries_on_branch(&query, &BranchBounds::default())
            .await
    }
}

/// Build the default durable overflow callback used by harnesses that have a
/// real provider stream and enabled compaction. The rich loop deliberately
/// remains session-agnostic; this callback is the boundary that records the
/// failed response, runs the real summarizer, appends the compaction entry,
/// and rebuilds the provider context from the resulting branch.
#[allow(clippy::too_many_arguments)]
fn automatic_overflow_recovery<F: FileSystem + 'static>(
    session: Arc<AsyncMutex<Session<F>>>,
    lane: String,
    stream_fn: crate::agent::StreamFn,
    stream_fn_with_options: Option<crate::agent::StreamFnWithOptions>,
    settings: CompactionSettings,
    retry_policy: RetryPolicy,
    thinking_level: ModelThinkingLevel,
    projectors: BTreeMap<String, EntryProjector>,
) -> (OverflowRecoveryHook, AutomaticOverflowPersistence) {
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let persisted_for_hook = persisted.clone();
    let hook: OverflowRecoveryHook = Arc::new(move |request, signal| {
        let session = session.clone();
        let lane = lane.clone();
        let stream_fn = stream_fn.clone();
        let stream_fn_with_options = stream_fn_with_options.clone();
        let settings = settings.clone();
        let retry_policy = retry_policy.clone();
        let thinking_level = thinking_level.as_str().to_string();
        let projectors = projectors.clone();
        let persisted = persisted_for_hook.clone();
        Box::pin(async move {
            let mut session = session.lock().await;
            let entries = lane_entries(&mut session, &lane)
                .await
                .map_err(|error| error.to_string())?;
            let existing = context_with_projectors(&entries, &projectors).messages;
            // The rich loop adds the current prompt to its active context
            // before the provider call, while the session owner persists the
            // whole run only after the loop returns. Therefore the durable
            // branch is the prefix of `retry_messages`, not necessarily the
            // whole request context. Failing closed on any mismatch avoids
            // duplicating old messages or silently dropping history.
            if request.retry_messages.len() < existing.len()
                || request.retry_messages[..existing.len()] != existing
                || request.durable_messages.len() < request.retry_messages.len()
                || request.durable_messages[..request.retry_messages.len()]
                    != request.retry_messages
            {
                return Err(
                    "overflow recovery context diverged from the durable session".to_string(),
                );
            }
            for message in request
                .durable_messages
                .iter()
                .skip(existing.len())
                .cloned()
            {
                let entry = EntryNoStats::Message {
                    id: crate::session::new_id(),
                    message: message.clone(),
                    terminate: None,
                };
                session
                    .append_entry(entry, &lane)
                    .await
                    .map_err(|error| error.to_string())?;
                persisted
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(message);
            }

            let entries = lane_entries(&mut session, &lane)
                .await
                .map_err(|error| error.to_string())?;
            let preparation = crate::harness::compaction::prepare_compaction(&entries, &settings)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "overflow recovery had no compaction preparation".to_string())?;
            let summary_stream = stream_fn.clone();
            let summary_stream_with_options = stream_fn_with_options.clone();
            let summary_models = SimpleModels::new(move |model, context, options| {
                let stream = summary_stream_with_options
                    .as_ref()
                    .map(|stream_fn| stream_fn(model, context, options))
                    .unwrap_or_else(|| summary_stream(model, context));
                Box::pin(async move { stream.collect().await.1 })
            });
            let compacted = crate::harness::compaction::compact(
                &preparation,
                &summary_models,
                &request.model,
                None,
                signal.as_ref(),
                Some(&thinking_level),
                Some(&retry_policy),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
            let details = compacted.details.as_ref().map(|details| {
                serde_json::json!({
                    "readFiles": details.read_files,
                    "modifiedFiles": details.modified_files,
                })
            });
            session
                .append_entry(
                    EntryNoStats::Compaction {
                        id: crate::session::new_id(),
                        summary: compacted.summary,
                        retained_tail: compacted.retained_tail,
                        tokens_before: compacted.tokens_before,
                        details,
                        usage: compacted.usage,
                    },
                    &lane,
                )
                .await
                .map_err(|error| error.to_string())?;

            let rebuilt = lane_entries(&mut session, &lane)
                .await
                .map_err(|error| error.to_string())?;
            let mut context = request.context;
            context.messages = context_with_projectors(&rebuilt, &projectors).messages;
            Ok(crate::rich_agent::OverflowRecoveryResult { context })
        })
    });
    (hook, persisted)
}

fn build_harness_agent(config: HarnessAgentConfig<'_>) -> Option<Arc<Agent>> {
    config.stream_fn.map(|stream_fn| {
        let mut agent = if let Some(stream_fn_with_options) = config.stream_fn_with_options {
            let mut agent = Agent::new(stream_fn);
            agent.set_stream_fn_with_options(stream_fn_with_options);
            agent
        } else {
            Agent::new(stream_fn)
        };
        let mut stream_options = config.stream_options.clone();
        if let Some(provider_context) = config.telemetry_context.provider_context() {
            stream_options.base.base.telemetry_context = Some(provider_context);
        }
        agent.set_stream_options(stream_options);
        agent.set_retry_policy(Some(config.retry_policy.clone()));
        if let Some(to_provider_messages) = config.to_provider_messages {
            agent.set_convert_to_llm(to_provider_messages);
        }
        let configured_tools: Vec<AgentTool> = if config.active_tool_names.is_empty() {
            config
                .tools
                .iter()
                .map(HarnessTool::to_agent_tool)
                .collect()
        } else {
            config
                .tools
                .iter()
                .filter(|tool| {
                    config
                        .active_tool_names
                        .iter()
                        .any(|name| name == tool.name())
                })
                .map(HarnessTool::to_agent_tool)
                .collect()
        };
        {
            let mut state = agent.state();
            state.model = config.model.clone();
            state.system_prompt = config.system_prompt.to_string();
            state.thinking_level = reasoning_level(config.thinking_level);
            state.set_tools(configured_tools);
        }
        agent.set_reasoning(reasoning_level(config.thinking_level));
        agent.set_steering_mode(match config.steering_mode {
            QueueMode::All => crate::rich_agent::QueueMode::All,
            QueueMode::OneAtATime => crate::rich_agent::QueueMode::OneAtATime,
        });
        agent.set_follow_up_mode(match config.follow_up_mode {
            QueueMode::All => crate::rich_agent::QueueMode::All,
            QueueMode::OneAtATime => crate::rich_agent::QueueMode::OneAtATime,
        });
        agent.set_block_images(config.block_images);
        agent.set_tool_result_image_options(config.tool_result_image_options);
        if let Some(overflow_recovery) = config.overflow_recovery {
            agent.set_overflow_recovery(overflow_recovery);
        }
        if let Some(before_tool_call) = config.before_tool_call {
            agent.set_before_tool_call(before_tool_call);
        }
        if let Some(after_tool_call) = config.after_tool_call {
            agent.set_after_tool_call(after_tool_call);
        }
        if let Some(ToolExecution::Sequential) = config.tool_execution {
            agent.set_tool_execution(crate::rich_agent::ToolExecutionMode::Sequential);
        }
        Arc::new(agent)
    })
}

fn lifecycle_outcome(messages: &[AgentMessage]) -> EventOutcome {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Core(pi_ai::types::Message::Assistant(assistant)) => {
                Some(match assistant.stop_reason() {
                    Some(pi_ai::types::StopReason::Aborted) => EventOutcome::Aborted,
                    Some(pi_ai::types::StopReason::Error) => EventOutcome::Failed,
                    _ => EventOutcome::Completed,
                })
            }
            _ => None,
        })
        .unwrap_or(EventOutcome::Completed)
}

fn lifecycle_error_message(messages: &[AgentMessage]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Core(pi_ai::types::Message::Assistant(assistant))
                if assistant.stop_reason() == Some(pi_ai::types::StopReason::Error) =>
            {
                assistant.error_message().map(str::to_string)
            }
            _ => None,
        })
        .unwrap_or_else(|| "provider request failed".to_string())
}

fn operation_kind(intent: &OperationIntent) -> OperationKind {
    match intent {
        OperationIntent::Run { .. } => OperationKind::Run,
        OperationIntent::Compaction { .. } => OperationKind::Compaction,
        OperationIntent::Navigation { .. } => OperationKind::Navigation,
    }
}

fn reasoning_level(level: ModelThinkingLevel) -> Option<pi_ai::types::ThinkingLevel> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(pi_ai::types::ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(pi_ai::types::ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(pi_ai::types::ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(pi_ai::types::ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(pi_ai::types::ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(pi_ai::types::ThinkingLevel::Max),
    }
}

fn suspended_operation_from_record(
    record: &LaneRecord,
    deferred: Option<DeferredHandle>,
    aborting: Option<AbortingPlan>,
) -> SuspendedOperation {
    let LaneRecord::OperationStarted {
        id,
        lane,
        timestamp,
        intent,
        ..
    } = record
    else {
        unreachable!("suspended operation must originate from operation_started")
    };
    let prompt = match intent {
        OperationIntent::Run {
            original_prompt, ..
        } => Some(original_prompt.clone()),
        OperationIntent::Compaction { .. } | OperationIntent::Navigation { .. } => None,
    };
    SuspendedOperation {
        lane: lane.clone(),
        kind: operation_kind(intent),
        id: id.clone(),
        started_at: *timestamp,
        reason: if deferred.is_some() {
            SuspensionReason::Deferred
        } else {
            SuspensionReason::Crash
        },
        prompt,
        deferred,
        aborting,
        missing: MissingIdentitiesInfo::default(),
    }
}

fn deferred_handle_for_open_run(
    records: &[LaneRecord],
    entries: &[Entry],
    lane: &str,
    run_id: &str,
) -> Option<DeferredHandle> {
    let result_entry_id = records.iter().rev().find_map(|record| match record {
        LaneRecord::StepAttempt {
            lane: record_lane,
            run_id: record_run_id,
            result_entry_id,
            ..
        } if record_lane == lane && record_run_id == run_id => Some(result_entry_id.as_str()),
        _ => None,
    })?;
    entries.iter().find_map(|entry| match entry {
        Entry::Message { id, message, .. } if id == result_entry_id => match message {
            AgentMessage::Core(Message::Assistant(assistant))
                if assistant.stop_reason() == Some(pi_ai::types::StopReason::Deferred) =>
            {
                assistant.deferred().cloned()
            }
            _ => None,
        },
        _ => None,
    })
}

fn queued_item_from_target(
    lane: &str,
    target: &serde_json::Value,
) -> Result<QueuedItem, HarnessError> {
    let entry = serde_json::from_value::<EntryNoStats>(target.clone()).map_err(|error| {
        HarnessError::fault(format!("invalid queued target in lane {lane}: {error}"))
    })?;
    match entry {
        EntryNoStats::Message { id, message, .. } => Ok(QueuedItem {
            entry_id: id,
            message,
        }),
        other => Err(HarnessError::fault(format!(
            "queued target {} in lane {lane} is not a message",
            other.id()
        ))),
    }
}

impl<F: FileSystem + 'static> std::fmt::Debug for AgentHarness<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHarness")
            .field("name", &self.name)
            .field("session", &"<session>")
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field("active_tool_names", &self.active_tool_names)
            .field(
                "tools",
                &self.tools.iter().map(|t| t.name()).collect::<Vec<_>>(),
            )
            .field("stream_options", &"<stream_options>")
            .field("retry_policy", &self.retry_policy)
            .field("compaction_settings", &self.compaction_settings)
            .field("steering_mode", &self.steering_mode)
            .field("follow_up_mode", &self.follow_up_mode)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl<F: FileSystem + 'static> AgentHarness<F> {
    /// Open a harness and restore the session's current context and open
    /// operation records. Completed records remain durable history; only an
    /// operation without a matching finish record is returned as suspended
    /// work.
    pub async fn create(
        mut options: AgentHarnessOptions<F>,
    ) -> Result<(AgentHarness<F>, Vec<SuspendedOperation>), HarnessError> {
        let session_id = options.session.get_metadata().await.id;
        let stream_options = options.stream_options.get_or_insert_with(Default::default);
        if stream_options.base.session_id.is_none() {
            stream_options.base.session_id = Some(session_id);
        }
        let lanes = options.session.get_lanes().await;
        let records = options
            .session
            .find_records(&RecordQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(HarnessError::from)?;
        let entries = options
            .session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(HarnessError::from)?;
        let materialized_entry_ids: HashSet<String> =
            entries.iter().map(|entry| entry.id().to_string()).collect();
        let mut open_records = Vec::new();
        for lane in &lanes {
            if let Some(record) = options
                .session
                .find_open_operations(&lane.lane, Some(1))
                .await
                .map_err(HarnessError::from)?
                .into_iter()
                .next()
            {
                open_records.push(record);
            }
        }
        let open_run_ids: BTreeMap<String, String> = open_records
            .iter()
            .filter_map(|record| match record {
                LaneRecord::OperationStarted { id, lane, .. } => Some((lane.clone(), id.clone())),
                _ => None,
            })
            .collect();
        let harness = AgentHarness::new(options);
        harness.refresh_agent_context().await?;
        harness.restore_durable_queues(&records, &materialized_entry_ids, &open_run_ids)?;
        harness.rebuild_agent_queue("steer");
        harness.rebuild_agent_queue("followUp");

        let mut suspended = Vec::with_capacity(open_records.len());
        for record in &open_records {
            let LaneRecord::OperationStarted {
                id, lane, intent, ..
            } = &record
            else {
                continue;
            };
            let aborting = records.iter().any(|candidate| {
                matches!(
                    candidate,
                    LaneRecord::AbortRequested {
                        lane: candidate_lane,
                        run_id,
                        ..
                    } if candidate_lane == lane && run_id == id
                )
            });
            let kind = operation_kind(intent);
            harness
                .active_operations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(
                    lane.clone(),
                    OperationInfo {
                        id: id.clone(),
                        kind,
                        status: if aborting {
                            OperationStatus::Aborting
                        } else {
                            OperationStatus::Suspended
                        },
                    },
                );
            let queues = harness.queue_snapshot_for_lane(lane);
            let deferred = deferred_handle_for_open_run(&records, &entries, lane, id);
            suspended.push(suspended_operation_from_record(
                record,
                deferred,
                aborting.then(|| AbortingPlan {
                    steer: queues.steer.into_iter().map(|item| item.message).collect(),
                    follow_up: queues
                        .follow_up
                        .into_iter()
                        .map(|item| item.message)
                        .collect(),
                }),
            ));
        }
        Ok((harness, suspended))
    }

    fn new(options: AgentHarnessOptions<F>) -> Self {
        let closed = Arc::new(RwLock::new(false));
        let session = Arc::new(AsyncMutex::new(options.session));
        let active_tool_names = options.active_tool_names.clone().unwrap_or_else(|| {
            options
                .tools
                .as_ref()
                .map(|tools| tools.iter().map(|t| t.name().to_string()).collect())
                .unwrap_or_default()
        });
        let retry_policy = options.retry.clone().unwrap_or(RetryPolicy {
            enabled: false,
            max_retries: 0,
            base_delay_ms: 1000,
        });
        let compaction_settings = options
            .compaction
            .clone()
            .unwrap_or(DEFAULT_COMPACTION_SETTINGS);
        let telemetry_context = options.context.clone().unwrap_or_default();
        let tools = options.tools.clone().unwrap_or_default();
        let system_prompt = options.system_prompt.clone().unwrap_or_default();
        let entry_projectors = options.entry_projectors.clone().unwrap_or_default();
        let stream_fn = options.stream_fn.clone();
        let stream_fn_with_options = options.stream_fn_with_options.clone();
        let stream_options = options.stream_options.clone().unwrap_or_default();
        let (overflow_recovery, automatic_overflow_persisted, automatic_overflow_recovery) =
            if let Some(overflow_recovery) = options.overflow_recovery.clone() {
                (Some(overflow_recovery), None, false)
            } else if compaction_settings.enabled {
                if let Some(stream_fn) = stream_fn.clone() {
                    let (hook, persisted) = automatic_overflow_recovery(
                        session.clone(),
                        "main".to_string(),
                        stream_fn,
                        stream_fn_with_options.clone(),
                        compaction_settings.clone(),
                        retry_policy.clone(),
                        options.thinking_level.unwrap_or(ModelThinkingLevel::Off),
                        entry_projectors.clone(),
                    );
                    (Some(hook), Some(persisted), true)
                } else {
                    (None, None, false)
                }
            } else {
                (None, None, false)
            };
        let agent = build_harness_agent(HarnessAgentConfig {
            stream_fn: stream_fn.clone(),
            stream_fn_with_options: stream_fn_with_options.clone(),
            overflow_recovery: overflow_recovery.clone(),
            before_tool_call: options.before_tool_call.clone(),
            after_tool_call: options.after_tool_call.clone(),
            model: &options.model,
            system_prompt: &system_prompt,
            tools: &tools,
            active_tool_names: &active_tool_names,
            thinking_level: options.thinking_level.unwrap_or(ModelThinkingLevel::Off),
            steering_mode: options.steering_mode.unwrap_or_default(),
            follow_up_mode: options.follow_up_mode.unwrap_or_default(),
            to_provider_messages: options.to_provider_messages.clone(),
            block_images: options.block_images,
            tool_result_image_options: options.tool_result_image_options,
            tool_execution: options.tool_execution,
            stream_options: &stream_options,
            retry_policy: &retry_policy,
            telemetry_context: &telemetry_context,
        });
        Self {
            name: "main".to_string(),
            session,
            model: options.model,
            thinking_level: options.thinking_level.unwrap_or(ModelThinkingLevel::Off),
            active_tool_names,
            tools,
            resources: options.resources.clone().unwrap_or_default(),
            stream_options,
            retry_policy,
            compaction_settings,
            steering_mode: options.steering_mode.unwrap_or_default(),
            follow_up_mode: options.follow_up_mode.unwrap_or_default(),
            drive: options.drive.unwrap_or_default(),
            agent,
            stream_fn,
            stream_fn_with_options,
            overflow_recovery,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            automatic_overflow_persisted,
            automatic_overflow_recovery,
            system_prompt,
            block_images: options.block_images,
            tool_result_image_options: options.tool_result_image_options,
            tool_execution: options.tool_execution,
            telemetry_context,
            tool_context: options.tool_context,
            entry_projectors,
            to_provider_messages: options.to_provider_messages,
            event_bus: Arc::new(std::sync::Mutex::new(HarnessEventBus::new())),
            queue_state: Arc::new(Mutex::new(BTreeMap::from([(
                "main".to_string(),
                LaneQueues::default(),
            )]))),
            queue_item_status: Arc::new(Mutex::new(BTreeMap::new())),
            active_operations: Arc::new(Mutex::new(BTreeMap::new())),
            operation_signals: Arc::new(Mutex::new(BTreeMap::new())),
            pending_actions: Arc::new(Mutex::new(BTreeMap::new())),
            hooks: HookEventRegistry::new(closed.clone()),
            events: HookEventRegistry::new(closed.clone()),
            closed,
        }
    }

    /// The underlying durable session tree (upstream aliases
    /// `durableSession` and `session` to the same object).
    pub fn session(&self) -> Arc<AsyncMutex<Session<F>>> {
        self.session.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.read().map(|b| *b).unwrap_or(false)
    }

    /// Subscribe to the integrated typed harness run lifecycle.
    pub fn subscribe_event(
        &mut self,
        event_type: &'static str,
        listener: HarnessEventListener,
    ) -> usize {
        self.event_bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .on(event_type, listener)
    }

    pub fn unsubscribe_event(&mut self, subscription_id: usize) {
        self.event_bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unsubscribe(subscription_id);
    }

    fn runtime_required<T>(&self, operation: &str) -> Result<T, HarnessError> {
        Err(HarnessError::fault(format!(
            "AgentHarness.{operation} requires a configured stream function"
        )))
    }

    fn emit_harness_event(&self, event: HarnessEvent) {
        let payload = match &event {
            HarnessEvent::RunStart(value) => serde_json::json!({
                "type": "run_start",
                "lane": value.lane,
                "runId": value.run_id,
            }),
            HarnessEvent::RunEnd(value) => serde_json::json!({
                "type": "run_end",
                "lane": value.lane,
                "runId": value.run_id,
                "outcome": value.outcome.as_str(),
                "leafId": value.leaf_id,
            }),
        };
        let event_name = event.event_type();
        self.event_bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .emit(&event);
        self.events.emit(event_name, &payload);
    }

    fn emit_hook(&self, name: &str, payload: serde_json::Value) {
        // Hook failures are isolated from the operation. This is the same
        // passive safety boundary as events in the upstream API; the Rust
        // callback type cannot return a hook result or an async error.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.hooks.emit(name, &payload);
        }));
    }

    fn reserve_operation(
        &self,
        id: &str,
        kind: OperationKind,
        action: ActionInfo,
    ) -> Result<Arc<AtomicBool>, HarnessError> {
        let signal = Arc::new(AtomicBool::new(false));
        let mut active_operations = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(operation) = active_operations.get(&self.name) {
            return Err(HarnessError::lane_busy(
                &self.name,
                &operation.id,
                operation.kind.as_str(),
            ));
        }
        active_operations.insert(
            self.name.clone(),
            OperationInfo {
                id: id.to_string(),
                kind,
                status: OperationStatus::Running,
            },
        );
        drop(active_operations);
        self.operation_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.to_string(), signal.clone());
        self.pending_actions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(self.name.clone(), VecDeque::from([action]));
        Ok(signal)
    }

    fn resume_operation(
        &self,
        id: &str,
        kind: OperationKind,
    ) -> Result<Arc<AtomicBool>, HarnessError> {
        let signal = Arc::new(AtomicBool::new(false));
        let mut active_operations = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(operation) = active_operations.get_mut(&self.name) else {
            return Err(HarnessError::nothing_to_resume(&self.name));
        };
        if operation.id != id || operation.kind != kind {
            return Err(HarnessError::lane_busy(
                &self.name,
                &operation.id,
                operation.kind.as_str(),
            ));
        }
        if operation.status == OperationStatus::Running {
            return Err(HarnessError::lane_busy(
                &self.name,
                &operation.id,
                operation.kind.as_str(),
            ));
        }
        operation.status = OperationStatus::Running;
        drop(active_operations);
        self.operation_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.to_string(), signal.clone());
        self.pending_actions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                self.name.clone(),
                VecDeque::from([ActionInfo::StreamAssistant {
                    step: kind.as_str().to_string(),
                    attempt: 1,
                }]),
            );
        Ok(signal)
    }

    fn release_operation(&self, id: &str) {
        self.active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.name);
        self.operation_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id);
        self.pending_actions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.name);
    }

    fn mark_queued_items(&self, queue: &str, status: QueueItemStatus) -> Vec<QueuedItem> {
        let items = {
            let mut queues = self
                .queue_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let lane_queues = queues.entry(self.name.clone()).or_default();
            match queue {
                "steer" => std::mem::take(&mut lane_queues.steer),
                "followUp" => std::mem::take(&mut lane_queues.follow_up),
                "nextRun" => std::mem::take(&mut lane_queues.next_run),
                _ => unreachable!("internal queue name: {queue}"),
            }
        };
        let mut statuses = self
            .queue_item_status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for item in &items {
            if let Some(current) = statuses.get_mut(&item.entry_id) {
                if *current == QueueItemStatus::Pending {
                    *current = status;
                }
            }
        }
        items
    }

    fn restore_queued_items(&self, queue: &str, items: Vec<QueuedItem>) {
        if items.is_empty() {
            return;
        }
        let mut queues = self
            .queue_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let lane_queues = queues.entry(self.name.clone()).or_default();
        match queue {
            "steer" => lane_queues.steer.extend(items.iter().cloned()),
            "followUp" => lane_queues.follow_up.extend(items.iter().cloned()),
            "nextRun" => lane_queues.next_run.extend(items.iter().cloned()),
            _ => unreachable!("internal queue name: {queue}"),
        }
        let mut statuses = self
            .queue_item_status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for item in items {
            statuses.insert(item.entry_id, QueueItemStatus::Pending);
        }
    }

    fn restore_durable_queues(
        &self,
        records: &[LaneRecord],
        materialized_entry_ids: &HashSet<String>,
        open_run_ids: &BTreeMap<String, String>,
    ) -> Result<(), HarnessError> {
        let mut queues_by_lane: BTreeMap<String, LaneQueues> = BTreeMap::new();
        let mut statuses = BTreeMap::new();

        for record in records {
            match record {
                LaneRecord::QueueEnqueued {
                    lane,
                    queue,
                    run_id,
                    target,
                    ..
                } => {
                    let queue_is_supported =
                        matches!(queue.as_str(), "steer" | "followUp" | "nextRun");
                    if !queue_is_supported {
                        continue;
                    }
                    if matches!(queue.as_str(), "steer" | "followUp")
                        && open_run_ids.get(lane) != Some(run_id)
                    {
                        continue;
                    }
                    let item = queued_item_from_target(lane, target)?;
                    let entry_id = item.entry_id.clone();
                    if materialized_entry_ids.contains(&entry_id) {
                        statuses.insert(entry_id, QueueItemStatus::Consumed);
                        continue;
                    }
                    let lane_queues = queues_by_lane.entry(lane.clone()).or_default();
                    let destination = match queue.as_str() {
                        "steer" => &mut lane_queues.steer,
                        "followUp" => &mut lane_queues.follow_up,
                        "nextRun" => &mut lane_queues.next_run,
                        _ => unreachable!("unsupported queue filtered above: {queue}"),
                    };
                    destination.push(item);
                    statuses.insert(entry_id, QueueItemStatus::Pending);
                }
                LaneRecord::QueueCancelled { lane, entry_id, .. } => {
                    let mut removed = false;
                    if let Some(lane_queues) = queues_by_lane.get_mut(lane) {
                        for items in [
                            &mut lane_queues.steer,
                            &mut lane_queues.follow_up,
                            &mut lane_queues.next_run,
                        ] {
                            if let Some(index) =
                                items.iter().position(|item| item.entry_id == *entry_id)
                            {
                                items.remove(index);
                                removed = true;
                                break;
                            }
                        }
                    }
                    if removed || statuses.contains_key(entry_id) {
                        statuses.insert(entry_id.clone(), QueueItemStatus::Cleared);
                    }
                }
                _ => {}
            }
        }

        let mut queue_state = self
            .queue_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (lane, lane_queues) in queues_by_lane {
            let destination = queue_state.entry(lane).or_default();
            destination.steer.extend(lane_queues.steer);
            destination.follow_up.extend(lane_queues.follow_up);
            destination.next_run.extend(lane_queues.next_run);
        }
        drop(queue_state);
        self.queue_item_status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(statuses);
        Ok(())
    }

    fn queue_snapshot_for_lane(&self, lane: &str) -> LaneQueues {
        self.queue_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(lane)
            .cloned()
            .unwrap_or_default()
    }

    fn queued_entry_ids_for_messages(
        messages: &[AgentMessage],
        input_prompt_count: usize,
        next_run_items: &[QueuedItem],
        run_queues: &LaneQueues,
    ) -> Vec<Option<String>> {
        let mut entry_ids = vec![None; messages.len()];
        let mut used = HashSet::new();
        for (index, item) in next_run_items.iter().enumerate() {
            if messages.get(index) == Some(&item.message) {
                entry_ids[index] = Some(item.entry_id.clone());
                used.insert(item.entry_id.clone());
            }
        }

        let queued_items = run_queues.steer.iter().chain(run_queues.follow_up.iter());
        for (index, message) in messages.iter().enumerate().skip(input_prompt_count) {
            if let Some(item) = queued_items
                .clone()
                .find(|item| !used.contains(&item.entry_id) && item.message == *message)
            {
                entry_ids[index] = Some(item.entry_id.clone());
                used.insert(item.entry_id.clone());
            }
        }
        entry_ids
    }

    fn mark_completed_run_queues_consumed(&self) {
        self.mark_queued_items("steer", QueueItemStatus::Consumed);
        self.mark_queued_items("followUp", QueueItemStatus::Consumed);
    }

    fn mark_aborted_run_queues_cleared(&self) {
        self.mark_queued_items("steer", QueueItemStatus::Cleared);
        self.mark_queued_items("followUp", QueueItemStatus::Cleared);
    }

    fn rebuild_agent_queue(&self, queue: &str) {
        let Some(agent) = &self.agent else {
            return;
        };
        let messages: Vec<AgentMessage> = {
            let queues = self
                .queue_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let lane_queues = queues.get(&self.name);
            match queue {
                "steer" => lane_queues
                    .map(|queues| {
                        queues
                            .steer
                            .iter()
                            .map(|item| item.message.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                "followUp" => lane_queues
                    .map(|queues| {
                        queues
                            .follow_up
                            .iter()
                            .map(|item| item.message.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => unreachable!("only Agent-owned queues can be rebuilt: {queue}"),
            }
        };
        match queue {
            "steer" => {
                agent.clear_steering_queue();
                for message in messages {
                    agent.steer(message);
                }
            }
            "followUp" => {
                agent.clear_follow_up_queue();
                for message in messages {
                    agent.follow_up(message);
                }
            }
            _ => unreachable!("only Agent-owned queues can be rebuilt: {queue}"),
        }
    }

    fn set_operation_status(&self, status: OperationStatus) {
        if let Some(operation) = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&self.name)
        {
            operation.status = status;
        }
    }

    fn simple_models(&self) -> Option<SimpleModels> {
        let stream_fn = self.stream_fn.clone()?;
        Some(SimpleModels::new(move |model, context, _options| {
            let stream = stream_fn(model, context);
            Box::pin(async move { stream.collect().await.1 })
        }))
    }

    async fn refresh_agent_context(&self) -> Result<(), HarnessError> {
        let Some(agent) = &self.agent else {
            return Ok(());
        };
        let entries = self.transcript().await?;
        let mut context_options = crate::session::context::SessionContextBuildOptions::default();
        for (custom_type, projector) in &self.entry_projectors {
            let projector = projector.clone();
            context_options.entry_projectors.insert(
                custom_type.clone(),
                Box::new(move |entry, _index, _entries| Some(projector(entry))),
            );
        }
        let context = crate::session::context::build_session_context(&entries, &context_options);
        agent.state().set_messages(context.messages);
        Ok(())
    }

    async fn wait_for_signal(signal: Arc<AtomicBool>) {
        while !signal.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn finish_operation_record(
        &self,
        run_id: &str,
        outcome: &str,
        error: Option<crate::session::types::OperationError>,
    ) -> Result<(), HarnessError> {
        self.session
            .lock()
            .await
            .append_record(NewRecord::OperationFinished {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                run_id: run_id.to_string(),
                outcome: outcome.to_string(),
                error,
            })
            .await
            .map(|_| ())
            .map_err(HarnessError::from)
    }

    /// Run prompts through the configured stateful Agent and append the
    /// resulting messages to this lane's session branch.
    pub async fn run_prompt(
        &self,
        prompts: Vec<AgentMessage>,
    ) -> Result<Vec<AgentMessage>, HarnessError> {
        self.run_prompt_with_events(prompts)
            .await
            .map(|(messages, _)| messages)
    }

    /// Run prompts through the configured stateful Agent and return the
    /// durable message delta together with the rich events from that run.
    pub async fn run_prompt_with_events(
        &self,
        prompts: Vec<AgentMessage>,
    ) -> Result<(Vec<AgentMessage>, Vec<crate::rich_agent::RichAgentEvent>), HarnessError> {
        self.run_prompt_with_events_for_lane(prompts)
            .await
            .map(|(_, messages, events)| (messages, events))
    }

    async fn run_prompt_with_events_for_lane(
        &self,
        prompts: Vec<AgentMessage>,
    ) -> Result<
        (
            String,
            Vec<AgentMessage>,
            Vec<crate::rich_agent::RichAgentEvent>,
        ),
        HarnessError,
    > {
        self.run_prompt_with_events_for_lane_inner(prompts, None)
            .await
    }

    async fn run_prompt_with_events_for_existing_operation(
        &self,
        prompts: Vec<AgentMessage>,
        run_id: String,
    ) -> Result<
        (
            String,
            Vec<AgentMessage>,
            Vec<crate::rich_agent::RichAgentEvent>,
        ),
        HarnessError,
    > {
        self.run_prompt_with_events_for_lane_inner(prompts, Some(run_id))
            .await
    }

    async fn run_prompt_with_events_for_lane_inner(
        &self,
        prompts: Vec<AgentMessage>,
        resumed_run_id: Option<String>,
    ) -> Result<
        (
            String,
            Vec<AgentMessage>,
            Vec<crate::rich_agent::RichAgentEvent>,
        ),
        HarnessError,
    > {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let Some(agent) = self.agent.clone() else {
            return self.runtime_required("prompt");
        };
        if let Some(persisted) = &self.automatic_overflow_persisted {
            persisted
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
        let recovering = resumed_run_id.is_some();
        let source_leaf_id = if recovering {
            None
        } else {
            self.lane_leaf_id().await.map_err(HarnessError::from)?
        };
        let next_run_items = if recovering {
            Vec::new()
        } else {
            self.mark_queued_items("nextRun", QueueItemStatus::Consumed)
        };
        let prompts = next_run_items
            .iter()
            .map(|item| item.message.clone())
            .chain(prompts)
            .collect::<Vec<_>>();
        let input_prompt_count = prompts.len();
        let next_run_items_for_persist = next_run_items.clone();
        let run_id = resumed_run_id.unwrap_or_else(crate::session::new_id);
        let session_id = self.session.lock().await.get_metadata().await.id;
        self.emit_hook(
            if recovering {
                HookName::BeforeResume.as_str()
            } else {
                HookName::BeforeRun.as_str()
            },
            serde_json::json!({"lane": self.name, "runId": run_id, "prompt": prompts}),
        );
        if recovering {
            self.resume_operation(&run_id, OperationKind::Run)?;
        } else {
            if let Err(error) = self.reserve_operation(
                &run_id,
                OperationKind::Run,
                ActionInfo::StreamAssistant {
                    step: "assistant".to_string(),
                    attempt: 1,
                },
            ) {
                self.restore_queued_items("nextRun", next_run_items);
                return Err(error);
            }
            let start_record = self
                .session
                .lock()
                .await
                .append_record(NewRecord::OperationStarted {
                    id: run_id.clone(),
                    lane: self.name.clone(),
                    source_leaf_id,
                    intent: OperationIntent::Run {
                        original_prompt: prompts.clone(),
                        initial_messages: Vec::new(),
                        system_prompt_override: None,
                        resume_data: None,
                    },
                })
                .await;
            if let Err(error) = start_record {
                self.release_operation(&run_id);
                self.restore_queued_items("nextRun", next_run_items);
                return Err(HarnessError::from(error));
            }
        }
        self.emit_harness_event(HarnessEvent::RunStart(RunStartEvent {
            lane: self.name.clone(),
            run_id: run_id.clone(),
        }));

        let telemetry = self.telemetry_context.clone();
        let span_run_id = run_id.clone();
        let span_session_id = session_id;
        let span_lane = self.name.clone();
        let lane = self.name.clone();
        let session = self.session.clone();
        let queue_state = self.queue_state.clone();
        let automatic_overflow_persisted = self.automatic_overflow_persisted.clone();
        let run_result: Result<
            (Vec<AgentMessage>, Vec<crate::rich_agent::RichAgentEvent>),
            HarnessError,
        > = telemetry
            .start_span_async(
                SpanOptions {
                    name: "pi.harness.run".to_string(),
                    attributes: Some(BTreeMap::from([
                        (
                            "pi.session.id".to_string(),
                            serde_json::json!(span_session_id),
                        ),
                        ("pi.lane.name".to_string(), serde_json::json!(span_lane)),
                        (
                            "pi.operation.id".to_string(),
                            serde_json::json!(span_run_id),
                        ),
                        (
                            "pi.operation.recovery".to_string(),
                            serde_json::json!(recovering),
                        ),
                        ("pi.operation.kind".to_string(), serde_json::json!("run")),
                    ])),
                },
                move |span| async move {
                    span.add_event("run_start", None);
                    let (messages, events) = agent
                        .prompt_messages_with_events(prompts)
                        .await
                        .map_err(|error| HarnessError::fault(error.to_string()))?;
                    let run_queues = queue_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get(&lane)
                        .cloned()
                        .unwrap_or_default();
                    let queued_entry_ids = Self::queued_entry_ids_for_messages(
                        &messages,
                        input_prompt_count,
                        &next_run_items_for_persist,
                        &run_queues,
                    );
                    // The built-in recovery hook has already durably written
                    // the prompt and failed response before compacting. The
                    // rich loop returns those same occurrences as part of its
                    // delta, so consume exactly one matching occurrence for
                    // each hook-owned entry and append only the remainder.
                    let mut automatically_persisted = automatic_overflow_persisted
                        .as_ref()
                        .map(|persisted| {
                            std::mem::take(
                                &mut *persisted.lock().unwrap_or_else(|error| error.into_inner()),
                            )
                        })
                        .unwrap_or_default();
                    let mut session = session.lock().await;
                    for (index, message) in messages.iter().enumerate() {
                        if let Some(position) = automatically_persisted
                            .iter()
                            .position(|persisted| persisted == message)
                        {
                            automatically_persisted.remove(position);
                            continue;
                        }
                        if let Err(error) = session
                            .append_entry(
                                EntryNoStats::Message {
                                    id: queued_entry_ids
                                        .get(index)
                                        .and_then(|id| id.clone())
                                        .unwrap_or_else(crate::session::new_id),
                                    message: message.clone(),
                                    terminate: None,
                                },
                                &lane,
                            )
                            .await
                        {
                            span.set_status(SpanStatus::Error {
                                error: Some(SpanError {
                                    name: "SessionError".to_string(),
                                    message: error.to_string(),
                                }),
                            });
                            span.set_attributes(BTreeMap::from([(
                                "pi.operation.outcome".to_string(),
                                serde_json::json!(EventOutcome::Failed.as_str()),
                            )]));
                            span.add_event(
                                "run_end",
                                Some(BTreeMap::from([(
                                    "pi.operation.outcome".to_string(),
                                    serde_json::json!(EventOutcome::Failed.as_str()),
                                )])),
                            );
                            return Err(HarnessError::from(error));
                        }
                    }
                    let outcome = lifecycle_outcome(&messages);
                    if outcome == EventOutcome::Failed {
                        span.set_status(SpanStatus::Error {
                            error: Some(SpanError {
                                name: "ProviderError".to_string(),
                                message: lifecycle_error_message(&messages),
                            }),
                        });
                    }
                    span.set_attributes(BTreeMap::from([(
                        "pi.operation.outcome".to_string(),
                        serde_json::json!(outcome.as_str()),
                    )]));
                    span.add_event(
                        "run_end",
                        Some(BTreeMap::from([(
                            "pi.operation.outcome".to_string(),
                            serde_json::json!(outcome.as_str()),
                        )])),
                    );
                    Ok((messages, events))
                },
            )
            .await;

        let outcome = match &run_result {
            Ok((messages, _)) => lifecycle_outcome(messages),
            Err(_) => EventOutcome::Failed,
        };
        let operation_outcome = outcome.as_str().to_string();
        let operation_error = match &run_result {
            Ok((messages, _)) if outcome == EventOutcome::Failed => {
                Some(crate::session::types::OperationError {
                    code: "provider_error".to_string(),
                    message: lifecycle_error_message(messages),
                })
            }
            Err(error) => Some(crate::session::types::OperationError {
                code: "harness_error".to_string(),
                message: error.to_string(),
            }),
            _ => None,
        };
        let finish_result = self
            .session
            .lock()
            .await
            .append_record(NewRecord::OperationFinished {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                run_id: run_id.clone(),
                outcome: operation_outcome,
                error: operation_error,
            })
            .await;
        match outcome {
            EventOutcome::Completed => self.mark_completed_run_queues_consumed(),
            EventOutcome::Aborted => self.mark_aborted_run_queues_cleared(),
            EventOutcome::Failed => {}
        }
        self.emit_hook(
            HookName::BeforeRunEnd.as_str(),
            serde_json::json!({
                "lane": self.name,
                "runId": run_id,
                "outcome": outcome.as_str(),
            }),
        );
        self.release_operation(&run_id);
        let leaf_id = self.lane_leaf_id().await.ok().flatten().unwrap_or_default();
        self.emit_harness_event(HarnessEvent::RunEnd(RunEndEvent {
            lane: self.name.clone(),
            run_id: run_id.clone(),
            outcome,
            leaf_id,
        }));
        match finish_result {
            Ok(_) => run_result.map(|(messages, events)| (run_id, messages, events)),
            Err(error) => Err(HarnessError::from(error)),
        }
    }

    /// Snapshot the harness-owned durable transcript in chronological order.
    pub async fn transcript(&self) -> Result<Vec<Entry>, HarnessError> {
        let mut session = self.session.lock().await;
        let query = EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        };
        if self.name == "main" {
            session
                .find_entries(&query)
                .await
                .map_err(HarnessError::from)
        } else {
            session
                .view(&self.name)
                .find_entries_on_branch(&query, &BranchBounds::default())
                .await
                .map_err(HarnessError::from)
        }
    }

    async fn lane_leaf_id(&self) -> Result<Option<String>, SessionError> {
        let mut session = self.session.lock().await;
        if self.name == "main" {
            session.get_leaf_id().await
        } else {
            session.view(&self.name).get_leaf_id().await
        }
    }

    /// Append a provisioned entry to the harness-owned lane.
    pub async fn append_entry(&mut self, entry: EntryNoStats) -> Result<Entry, HarnessError> {
        self.session
            .lock()
            .await
            .append_entry(entry, &self.name)
            .await
            .map_err(HarnessError::from)
    }

    /// Replace the in-memory Agent transcript after a compaction boundary.
    pub async fn set_agent_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), HarnessError> {
        let Some(agent) = &self.agent else {
            return self.runtime_required("setAgentMessages");
        };
        agent.state().set_messages(messages);
        Ok(())
    }

    pub async fn agent_messages(&self) -> Result<Vec<AgentMessage>, HarnessError> {
        let Some(agent) = &self.agent else {
            return self.runtime_required("agentMessages");
        };
        Ok(agent.state().messages().to_vec())
    }

    /// Estimate the provider context represented by the live agent transcript.
    /// The estimate uses the latest valid assistant usage as an exact prefix
    /// and the shared character/image estimator for messages appended after
    /// that provider response, matching the compaction accounting path.
    pub async fn context_usage(
        &self,
    ) -> Result<crate::harness::compaction::ContextUsageEstimate, HarnessError> {
        let messages = self.agent_messages().await?;
        Ok(crate::harness::compaction::estimate_context_tokens(
            &messages,
        ))
    }

    /// Whether the live context has crossed the configured compaction
    /// threshold for the selected model. This is deliberately an observation
    /// API: callers can choose when to start a durable compaction operation,
    /// while overflow recovery remains automatic at the provider boundary.
    pub async fn needs_compaction(&self) -> Result<bool, HarnessError> {
        let usage = self.context_usage().await?;
        Ok(crate::harness::compaction::should_compact(
            usage.tokens,
            self.model.context_window,
            &self.compaction_settings,
        ))
    }

    /// Return the stateful agent handle so interactive callers can enqueue
    /// steering/follow-up messages while a harness run is in flight.
    pub fn agent_handle(&self) -> Option<Arc<crate::rich_agent::Agent>> {
        self.agent.clone()
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    async fn make_lane(&self, name: &str) -> Result<AgentHarness<F>, HarnessError> {
        self.queue_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(name.to_string())
            .or_default();
        let (overflow_recovery, automatic_overflow_persisted, automatic_overflow_recovery) =
            if name == self.name {
                (
                    self.overflow_recovery.clone(),
                    self.automatic_overflow_persisted.clone(),
                    self.automatic_overflow_recovery,
                )
            } else if self.automatic_overflow_recovery {
                let stream_fn = self
                    .stream_fn
                    .clone()
                    .expect("automatic overflow recovery requires a stream function");
                let (hook, persisted) = automatic_overflow_recovery(
                    self.session.clone(),
                    name.to_string(),
                    stream_fn,
                    self.stream_fn_with_options.clone(),
                    self.compaction_settings.clone(),
                    self.retry_policy.clone(),
                    self.thinking_level,
                    self.entry_projectors.clone(),
                );
                (Some(hook), Some(persisted), true)
            } else {
                (self.overflow_recovery.clone(), None, false)
            };
        let agent = if name == self.name {
            self.agent.clone()
        } else {
            build_harness_agent(HarnessAgentConfig {
                stream_fn: self.stream_fn.clone(),
                stream_fn_with_options: self.stream_fn_with_options.clone(),
                overflow_recovery: overflow_recovery.clone(),
                before_tool_call: self.before_tool_call.clone(),
                after_tool_call: self.after_tool_call.clone(),
                model: &self.model,
                system_prompt: &self.system_prompt,
                tools: &self.tools,
                active_tool_names: &self.active_tool_names,
                thinking_level: self.thinking_level,
                steering_mode: self.steering_mode,
                follow_up_mode: self.follow_up_mode,
                to_provider_messages: self.to_provider_messages.clone(),
                block_images: self.block_images,
                tool_result_image_options: self.tool_result_image_options,
                tool_execution: self.tool_execution,
                stream_options: &self.stream_options,
                retry_policy: &self.retry_policy,
                telemetry_context: &self.telemetry_context,
            })
        };
        let lane = AgentHarness {
            name: name.to_string(),
            session: self.session.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            active_tool_names: self.active_tool_names.clone(),
            tools: self.tools.clone(),
            resources: self.resources.clone(),
            stream_options: self.stream_options.clone(),
            retry_policy: self.retry_policy.clone(),
            compaction_settings: self.compaction_settings.clone(),
            steering_mode: self.steering_mode,
            follow_up_mode: self.follow_up_mode,
            drive: self.drive,
            agent,
            stream_fn: self.stream_fn.clone(),
            stream_fn_with_options: self.stream_fn_with_options.clone(),
            overflow_recovery,
            before_tool_call: self.before_tool_call.clone(),
            after_tool_call: self.after_tool_call.clone(),
            automatic_overflow_persisted,
            automatic_overflow_recovery,
            system_prompt: self.system_prompt.clone(),
            block_images: self.block_images,
            tool_result_image_options: self.tool_result_image_options,
            tool_execution: self.tool_execution,
            telemetry_context: self.telemetry_context.clone(),
            tool_context: self.tool_context.clone(),
            entry_projectors: self.entry_projectors.clone(),
            to_provider_messages: self.to_provider_messages.clone(),
            event_bus: self.event_bus.clone(),
            queue_state: self.queue_state.clone(),
            queue_item_status: self.queue_item_status.clone(),
            active_operations: self.active_operations.clone(),
            operation_signals: self.operation_signals.clone(),
            pending_actions: self.pending_actions.clone(),
            hooks: self.hooks.shared_with(),
            events: self.events.shared_with(),
            closed: self.closed.clone(),
        };
        lane.refresh_agent_context().await?;
        lane.rebuild_agent_queue("steer");
        lane.rebuild_agent_queue("followUp");
        Ok(lane)
    }

    /// Return a lane-bound harness view (upstream `lane(name)`).
    pub async fn lane(&self, name: &str) -> Result<Box<dyn AgentLane>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let exists = self
            .session
            .lock()
            .await
            .get_lanes()
            .await
            .into_iter()
            .any(|lane| lane.lane == name);
        if !exists {
            return Err(HarnessError::invalid_lane(name, "lane does not exist"));
        }
        Ok(Box::new(self.make_lane(name).await?))
    }

    /// Create a new lane (upstream `createLane(name, at)`) and return its
    /// independent agent view over the shared session tree.
    pub async fn create_lane(
        &self,
        name: &str,
        at: Option<&str>,
    ) -> Result<Box<dyn AgentLane>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        if name == "main" || name.is_empty() {
            return Err(HarnessError::invalid_lane(
                name,
                "lane name is reserved or empty",
            ));
        }
        self.session
            .lock()
            .await
            .create_lane(name, at)
            .await
            .map_err(|error| {
                if error.kind == crate::session::types::SessionErrorKind::AlreadyExists {
                    HarnessError::lane_exists(name)
                } else {
                    HarnessError::from(error)
                }
            })?;
        Ok(Box::new(self.make_lane(name).await?))
    }

    /// List the durable session lanes (upstream `lanes()`).
    pub async fn lanes(&self) -> Result<Vec<LaneInfo>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let operations = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        Ok(self
            .session
            .lock()
            .await
            .get_lanes()
            .await
            .into_iter()
            .map(|lane| LaneInfo {
                operation: operations.get(&lane.lane).cloned(),
                name: lane.lane,
                leaf_id: lane.leaf_id,
            })
            .collect())
    }

    async fn enqueue_message(&self, queue: &str, message: &AgentMessage) -> QueueResult {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let operation = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .cloned()
            .ok_or_else(|| HarnessError::no_active_run(&self.name))?;
        if operation.kind != OperationKind::Run || operation.status != OperationStatus::Running {
            return Err(HarnessError::no_active_run(&self.name));
        }
        let entry_id = crate::session::new_id();
        let target = serde_json::to_value(EntryNoStats::Message {
            id: entry_id.clone(),
            message: message.clone(),
            terminate: None,
        })
        .map_err(|error| HarnessError::fault(format!("serialize queue item: {error}")))?;
        self.session
            .lock()
            .await
            .append_record(NewRecord::QueueEnqueued {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                queue: queue.to_string(),
                run_id: operation.id,
                target,
            })
            .await
            .map_err(HarnessError::from)?;

        // The lower-level Agent owns the actual steering/follow-up drain. The
        // harness queue is a durable/snapshot mirror used for cancellation
        // and inspection; keep both sides in sync at enqueue time.
        if let Some(agent) = &self.agent {
            match queue {
                "steer" => agent.steer(message.clone()),
                "followUp" => agent.follow_up(message.clone()),
                "nextRun" => {}
                _ => unreachable!("internal queue name: {queue}"),
            }
        }

        let mut queues = self
            .queue_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let lane_queues = queues.entry(self.name.clone()).or_default();
        let item = QueuedItem {
            entry_id: entry_id.clone(),
            message: message.clone(),
        };
        match queue {
            "steer" => lane_queues.steer.push(item),
            "followUp" => lane_queues.follow_up.push(item),
            "nextRun" => lane_queues.next_run.push(item),
            _ => unreachable!("internal queue name: {queue}"),
        }
        self.queue_item_status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(entry_id.clone(), QueueItemStatus::Pending);
        Ok(entry_id)
    }

    async fn cancel_queue_item(&self, entry_id: &str) -> CancelQueuedResult {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let prior_status = self
            .queue_item_status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(entry_id)
            .copied();
        match prior_status {
            Some(QueueItemStatus::Consumed) => return Ok(CancelQueuedOutcome::AlreadyConsumed),
            Some(QueueItemStatus::Cleared) => return Ok(CancelQueuedOutcome::AlreadyCleared),
            None => return Err(HarnessError::unknown_queue_item(&self.name, entry_id)),
            Some(QueueItemStatus::Pending) => {}
        }
        let removed = {
            let mut queues = self
                .queue_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let lane_queues = queues.entry(self.name.clone()).or_default();
            let mut removed = None;
            for (queue, items) in [
                ("steer", &mut lane_queues.steer),
                ("followUp", &mut lane_queues.follow_up),
                ("nextRun", &mut lane_queues.next_run),
            ] {
                if let Some(index) = items.iter().position(|item| item.entry_id == entry_id) {
                    removed = Some((queue, items.remove(index)));
                    break;
                }
            }
            removed
        };
        let Some((queue, item)) = removed else {
            return Ok(CancelQueuedOutcome::AlreadyConsumed);
        };
        let run_id = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .map(|operation| operation.id.clone());
        let record = self
            .session
            .lock()
            .await
            .append_record(NewRecord::QueueCancelled {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                run_id,
                entry_id: entry_id.to_string(),
            })
            .await;
        if let Err(error) = record {
            let mut queues = self
                .queue_state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let lane_queues = queues.entry(self.name.clone()).or_default();
            match queue {
                "steer" => lane_queues.steer.push(item),
                "followUp" => lane_queues.follow_up.push(item),
                "nextRun" => lane_queues.next_run.push(item),
                _ => unreachable!("internal queue name: {queue}"),
            }
            self.queue_item_status
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(entry_id.to_string(), QueueItemStatus::Pending);
            return Err(HarnessError::from(error));
        }
        self.queue_item_status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(entry_id.to_string(), QueueItemStatus::Cleared);
        if queue == "steer" || queue == "followUp" {
            self.rebuild_agent_queue(queue);
        }
        Ok(CancelQueuedOutcome::Cancelled)
    }

    fn prompt_with_images(text: &str, images: &[ImageContent]) -> AgentMessage {
        let mut blocks = vec![ContentBlock::text(text.to_string())];
        blocks.extend(images.iter().cloned().map(|image| ContentBlock::Image {
            data: image.data,
            mime_type: image.mime_type,
        }));
        AgentMessage::Core(Message::User(UserContent::blocks(
            blocks,
            pi_ai::types::now_ms(),
        )))
    }

    async fn prompt_resource(&self, prompt: String) -> Result<RunResultValue, HarnessError> {
        self.prompt_result(vec![Self::prompt_with_images(&prompt, &[])])
            .await
    }

    async fn prompt_result(
        &self,
        prompts: Vec<AgentMessage>,
    ) -> Result<RunResultValue, HarnessError> {
        let (run_id, messages, _) = self.run_prompt_with_events_for_lane(prompts).await?;
        self.run_result_from_messages(run_id, messages).await
    }

    async fn run_result_from_messages(
        &self,
        run_id: String,
        messages: Vec<AgentMessage>,
    ) -> Result<RunResultValue, HarnessError> {
        let final_message = messages.iter().rev().find_map(|message| match message {
            AgentMessage::Core(Message::Assistant(assistant)) => Some(assistant.clone()),
            _ => None,
        });
        let Some(final_message) = final_message else {
            return Err(HarnessError::fault("run produced no assistant message"));
        };
        let leaf_id = self.lane_leaf_id().await?.unwrap_or_default();
        let final_entry_id = leaf_id.clone();
        let outcome = match final_message.stop_reason() {
            Some(pi_ai::types::StopReason::Aborted) => RunOutcome::Aborted {
                leaf_id,
                final_entry_id,
                final_message,
            },
            Some(pi_ai::types::StopReason::Error) => RunOutcome::Failed {
                leaf_id,
                error: OperationError {
                    code: "provider_error".to_string(),
                    message: final_message
                        .error_message()
                        .map(str::to_string)
                        .unwrap_or_else(|| "provider request failed".to_string()),
                },
                final_entry_id: Some(final_entry_id),
                final_message: Some(final_message),
            },
            _ => RunOutcome::Completed {
                leaf_id,
                final_entry_id,
                final_message,
            },
        };
        Ok(RunResultValue { run_id, outcome })
    }
}

#[async_trait]
impl<F: FileSystem + 'static> AgentLane for AgentHarness<F> {
    fn lane_name(&self) -> &str {
        &self.name
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.lane_leaf_id().await
    }

    async fn prompt_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<RunResultValue, HarnessError> {
        self.prompt_result(vec![Self::prompt_with_images(text, images)])
            .await
    }

    async fn prompt_messages(
        &self,
        messages: &[AgentMessage],
    ) -> Result<RunResultValue, HarnessError> {
        self.prompt_result(messages.to_vec()).await
    }

    async fn skill(
        &self,
        name: &str,
        additional_instructions: Option<&str>,
    ) -> Result<RunResultValue, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        // A configured stream is required to execute the formatted resource
        // prompt; lookup errors remain tagged and deterministic.
        if self.agent.is_none() {
            return self.runtime_required("skill");
        }
        let skill = self
            .resources
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| HarnessError::unknown_skill(name))?;
        let prompt =
            crate::harness::skills::format_skill_invocation(skill, additional_instructions);
        self.prompt_resource(prompt).await
    }

    async fn prompt_from_template(
        &self,
        name: &str,
        args: Option<&[String]>,
    ) -> Result<RunResultValue, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        if self.agent.is_none() {
            return self.runtime_required("promptFromTemplate");
        }
        let template = self
            .resources
            .prompt_templates
            .iter()
            .find(|template| template.name == name)
            .ok_or_else(|| HarnessError::unknown_template(name))?;
        let prompt = crate::harness::prompt_templates::format_prompt_template_invocation(
            template,
            args.unwrap_or(&[]),
        );
        self.prompt_resource(prompt).await
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResultValue, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let entries = self.transcript().await?;
        let preparation =
            crate::harness::compaction::prepare_compaction(&entries, &self.compaction_settings)
                .map_err(|error| HarnessError::fault(error.to_string()))?;
        let Some(preparation) = preparation else {
            return Err(HarnessError::nothing_to_compact(&self.name));
        };
        if preparation.messages_to_summarize.is_empty()
            && preparation.turn_prefix_messages.is_empty()
        {
            return Err(HarnessError::nothing_to_compact(&self.name));
        }
        let Some(models) = self.simple_models() else {
            return self.runtime_required("compact");
        };

        let existing_operation = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .cloned();
        let recovered_compaction = if existing_operation.as_ref().is_some_and(|operation| {
            operation.kind == OperationKind::Compaction
                && operation.status == OperationStatus::Suspended
        }) {
            let operation_id = existing_operation
                .as_ref()
                .expect("checked above")
                .id
                .clone();
            let records = self
                .session
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    run_id: Some(operation_id.clone()),
                    ..Default::default()
                })
                .await
                .map_err(HarnessError::from)?;
            records.into_iter().find_map(|record| {
                let LaneRecord::OperationStarted {
                    intent:
                        OperationIntent::Compaction {
                            custom_instructions,
                            result_entry_id,
                        },
                    ..
                } = record
                else {
                    return None;
                };
                Some((operation_id.clone(), custom_instructions, result_entry_id))
            })
        } else {
            None
        };
        let recovering = recovered_compaction.is_some();
        let run_id = recovered_compaction
            .as_ref()
            .map(|(run_id, _, _)| run_id.clone())
            .unwrap_or_else(crate::session::new_id);
        let result_entry_id = recovered_compaction
            .as_ref()
            .map(|(_, _, entry_id)| entry_id.clone())
            .unwrap_or_else(crate::session::new_id);
        let recovered_instructions = recovered_compaction
            .as_ref()
            .and_then(|(_, instructions, _)| instructions.as_deref());
        let effective_instructions = custom_instructions.or(recovered_instructions);
        let source_leaf_id = self.lane_leaf_id().await.map_err(HarnessError::from)?;
        let signal = if recovering {
            self.resume_operation(&run_id, OperationKind::Compaction)?
        } else {
            let signal = self.reserve_operation(
                &run_id,
                OperationKind::Compaction,
                ActionInfo::StreamAssistant {
                    step: "compaction".to_string(),
                    attempt: 1,
                },
            )?;
            let start_record = self
                .session
                .lock()
                .await
                .append_record(NewRecord::OperationStarted {
                    id: run_id.clone(),
                    lane: self.name.clone(),
                    source_leaf_id,
                    intent: OperationIntent::Compaction {
                        custom_instructions: effective_instructions.map(str::to_string),
                        result_entry_id: result_entry_id.clone(),
                    },
                })
                .await;
            if let Err(error) = start_record {
                self.release_operation(&run_id);
                return Err(HarnessError::from(error));
            }
            signal
        };
        self.emit_hook(
            HookName::BeforeCompaction.as_str(),
            serde_json::json!({
                "lane": self.name,
                "runId": run_id,
                "customInstructions": effective_instructions,
            }),
        );

        let compaction_result = tokio::select! {
            result = crate::harness::compaction::compact(
                &preparation,
                &models,
                &self.model,
                effective_instructions,
                Some(&signal),
                Some(self.thinking_level.as_str()),
                Some(&self.retry_policy),
                None,
            ) => result,
            _ = Self::wait_for_signal(signal.clone()) => Err(crate::harness::CompactionError::new(
                "aborted",
                "Compaction aborted",
            )),
        };

        let result = match compaction_result {
            Ok(result) if !signal.load(Ordering::SeqCst) => result,
            Ok(_) => {
                let error = crate::harness::CompactionError::new("aborted", "Compaction aborted");
                self.finish_operation_record(
                    &run_id,
                    "aborted",
                    Some(crate::session::types::OperationError {
                        code: error.code.to_string(),
                        message: error.message.clone(),
                    }),
                )
                .await?;
                self.release_operation(&run_id);
                let leaf_id = self.lane_leaf_id().await.map_err(HarnessError::from)?;
                return Ok(CompactionResultValue {
                    run_id,
                    outcome: CompactionOutcome::Aborted {
                        leaf_id: leaf_id.unwrap_or_default(),
                    },
                });
            }
            Err(error) => {
                let outcome = if error.code == "aborted" {
                    "aborted"
                } else {
                    "failed"
                };
                self.finish_operation_record(
                    &run_id,
                    outcome,
                    Some(crate::session::types::OperationError {
                        code: error.code.to_string(),
                        message: error.message.clone(),
                    }),
                )
                .await?;
                self.release_operation(&run_id);
                let leaf_id = self
                    .lane_leaf_id()
                    .await
                    .map_err(HarnessError::from)?
                    .unwrap_or_default();
                return Ok(CompactionResultValue {
                    run_id,
                    outcome: if error.code == "aborted" {
                        CompactionOutcome::Aborted { leaf_id }
                    } else {
                        CompactionOutcome::Failed {
                            leaf_id,
                            error: OperationError {
                                code: error.code.to_string(),
                                message: error.message,
                            },
                        }
                    },
                });
            }
        };

        let details = result.details.as_ref().map(|details| {
            serde_json::json!({
                "readFiles": details.read_files,
                "modifiedFiles": details.modified_files,
            })
        });
        let entry = self
            .session
            .lock()
            .await
            .append_entry(
                EntryNoStats::Compaction {
                    id: result_entry_id,
                    summary: result.summary,
                    retained_tail: result.retained_tail,
                    tokens_before: result.tokens_before,
                    details,
                    usage: result.usage,
                },
                &self.name,
            )
            .await;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let _ = self
                    .finish_operation_record(
                        &run_id,
                        "failed",
                        Some(crate::session::types::OperationError {
                            code: "session_error".to_string(),
                            message: error.to_string(),
                        }),
                    )
                    .await;
                self.release_operation(&run_id);
                return Err(HarnessError::from(error));
            }
        };
        let step_result = self
            .session
            .lock()
            .await
            .append_record(NewRecord::StepAttempt {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                run_id: run_id.clone(),
                step: "compaction".to_string(),
                attempt: 1,
                result_entry_id: entry.id().to_string(),
                compaction_reason: Some("manual".to_string()),
            })
            .await;
        if let Err(error) = step_result {
            let _ = self
                .finish_operation_record(
                    &run_id,
                    "failed",
                    Some(crate::session::types::OperationError {
                        code: "session_error".to_string(),
                        message: error.to_string(),
                    }),
                )
                .await;
            self.release_operation(&run_id);
            return Err(HarnessError::from(error));
        }
        self.refresh_agent_context().await?;
        self.finish_operation_record(&run_id, "completed", None)
            .await?;
        self.release_operation(&run_id);
        Ok(CompactionResultValue {
            run_id,
            outcome: CompactionOutcome::Completed {
                leaf_id: entry.id().to_string(),
                entry,
            },
        })
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    async fn navigate_tree(
        &self,
        target_id: Option<&str>,
        options: Option<&NavigateOptions>,
    ) -> Result<NavigationResultValue, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let options = options.cloned().unwrap_or_default();
        let old_leaf_id = self.lane_leaf_id().await.map_err(HarnessError::from)?;
        if let Some(target_id) = target_id {
            let exists = self
                .session
                .lock()
                .await
                .get_entry(target_id)
                .await
                .is_some();
            if !exists {
                return Err(HarnessError::unknown_target(target_id));
            }
            if old_leaf_id.as_deref() == Some(target_id) {
                return Err(HarnessError::invalid_navigation(
                    &self.name,
                    "target is already the current leaf",
                ));
            }
        }
        if options.summarize && target_id.is_none() {
            return Err(HarnessError::invalid_navigation(
                &self.name,
                "cannot summarize navigation to the root",
            ));
        }
        if options.label.is_some() && target_id.is_none() {
            return Err(HarnessError::invalid_navigation(
                &self.name,
                "cannot label the root navigation target",
            ));
        }

        let branch_entries = if options.summarize {
            let target_id = target_id.expect("summarized navigation has a target");
            let session = self.session.lock().await;
            crate::harness::compaction::collect_entries_for_branch_summary(
                &session,
                old_leaf_id.as_deref(),
                target_id,
            )
            .await
            .map_err(HarnessError::from)?
            .entries
        } else {
            Vec::new()
        };
        let models = if options.summarize {
            Some(self.simple_models().ok_or_else(|| {
                HarnessError::fault(
                    "AgentHarness.navigateTree summary requires a configured stream function",
                )
            })?)
        } else {
            None
        };
        let existing_operation = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .cloned();
        let recovered_navigation = if existing_operation.as_ref().is_some_and(|operation| {
            operation.kind == OperationKind::Navigation
                && operation.status == OperationStatus::Suspended
        }) {
            let operation_id = existing_operation
                .as_ref()
                .expect("checked above")
                .id
                .clone();
            let records = self
                .session
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    run_id: Some(operation_id.clone()),
                    ..Default::default()
                })
                .await
                .map_err(HarnessError::from)?;
            records.into_iter().find_map(|record| {
                let LaneRecord::OperationStarted {
                    intent:
                        OperationIntent::Navigation {
                            summary_entry_id, ..
                        },
                    ..
                } = record
                else {
                    return None;
                };
                Some((operation_id.clone(), summary_entry_id))
            })
        } else {
            None
        };
        let recovering = recovered_navigation.is_some();
        let run_id = recovered_navigation
            .as_ref()
            .map(|(run_id, _)| run_id.clone())
            .unwrap_or_else(crate::session::new_id);
        let summary_entry_id = recovered_navigation
            .as_ref()
            .and_then(|(_, entry_id)| entry_id.clone())
            .or_else(|| options.summarize.then(crate::session::new_id));
        let action = if options.summarize {
            ActionInfo::StreamAssistant {
                step: "navigation_summary".to_string(),
                attempt: 1,
            }
        } else {
            ActionInfo::MoveLane {
                to: target_id.map(str::to_string),
            }
        };
        let signal = if recovering {
            self.resume_operation(&run_id, OperationKind::Navigation)?
        } else {
            let signal = self.reserve_operation(&run_id, OperationKind::Navigation, action)?;
            let start_record = self
                .session
                .lock()
                .await
                .append_record(NewRecord::OperationStarted {
                    id: run_id.clone(),
                    lane: self.name.clone(),
                    source_leaf_id: old_leaf_id.clone(),
                    intent: OperationIntent::Navigation {
                        target_id: target_id.map(str::to_string),
                        summarize: options.summarize,
                        custom_instructions: options.custom_instructions.clone(),
                        label: options.label.clone(),
                        summary_entry_id: summary_entry_id.clone(),
                    },
                })
                .await;
            if let Err(error) = start_record {
                self.release_operation(&run_id);
                return Err(HarnessError::from(error));
            }
            signal
        };
        self.emit_hook(
            HookName::BeforeNavigation.as_str(),
            serde_json::json!({
                "lane": self.name,
                "runId": run_id,
                "targetId": target_id,
                "summarize": options.summarize,
            }),
        );

        let summary = if let Some(models) = models {
            let target_entries = branch_entries;
            let custom_instructions = options.custom_instructions.as_deref();
            let summary_options = crate::harness::compaction::GenerateBranchSummaryOptions {
                signal: Some(&signal),
                custom_instructions,
                replace_instructions: false,
                reserve_tokens: Some(self.compaction_settings.reserve_tokens),
                retry: Some(&self.retry_policy),
                callbacks: None,
            };
            tokio::select! {
                result = crate::harness::compaction::generate_branch_summary(
                    &target_entries,
                    &models,
                    &self.model,
                    &summary_options,
                ) => result.map(Some),
                _ = Self::wait_for_signal(signal.clone()) => Err(crate::harness::BranchSummaryError::new(
                    "aborted",
                    "Navigation summary aborted",
                )),
            }
        } else {
            Ok(None)
        };
        let summary = match summary {
            Ok(Some(summary)) => Some(summary),
            Ok(None) => None,
            Err(error) => {
                let outcome = if error.code == "aborted" {
                    "aborted"
                } else {
                    "failed"
                };
                self.finish_operation_record(
                    &run_id,
                    outcome,
                    Some(crate::session::types::OperationError {
                        code: error.code.to_string(),
                        message: error.message.clone(),
                    }),
                )
                .await?;
                self.release_operation(&run_id);
                let leaf_id = self.lane_leaf_id().await.map_err(HarnessError::from)?;
                return Ok(NavigationResultValue {
                    run_id,
                    outcome: if error.code == "aborted" {
                        NavigationOutcome::Aborted { leaf_id }
                    } else {
                        NavigationOutcome::Failed {
                            leaf_id,
                            error: OperationError {
                                code: error.code.to_string(),
                                message: error.message,
                            },
                        }
                    },
                });
            }
        };

        if signal.load(Ordering::SeqCst) {
            self.finish_operation_record(
                &run_id,
                "aborted",
                Some(crate::session::types::OperationError {
                    code: "aborted".to_string(),
                    message: "Navigation aborted".to_string(),
                }),
            )
            .await?;
            self.release_operation(&run_id);
            return Ok(NavigationResultValue {
                run_id,
                outcome: NavigationOutcome::Aborted {
                    leaf_id: old_leaf_id,
                },
            });
        }

        let moved = self
            .session
            .lock()
            .await
            .move_lane(&self.name, target_id)
            .await;
        if let Err(error) = moved {
            let _ = self
                .finish_operation_record(
                    &run_id,
                    "failed",
                    Some(crate::session::types::OperationError {
                        code: "navigation_error".to_string(),
                        message: error.to_string(),
                    }),
                )
                .await;
            self.release_operation(&run_id);
            return if let Some(target_id) = target_id {
                Err(HarnessError::unknown_target(target_id))
            } else {
                Err(HarnessError::from(error))
            };
        }

        let summary_entry = if let Some(summary) = summary {
            let from_id = old_leaf_id.clone().ok_or_else(|| {
                HarnessError::fault("summarized navigation requires a source leaf")
            })?;
            let entry = self
                .session
                .lock()
                .await
                .append_entry(
                    EntryNoStats::BranchSummary {
                        id: summary_entry_id
                            .clone()
                            .expect("summary entry id allocated for summarized navigation"),
                        from_id,
                        summary: summary.summary,
                        details: Some(serde_json::json!({
                            "readFiles": summary.read_files,
                            "modifiedFiles": summary.modified_files,
                        })),
                        usage: summary.usage,
                    },
                    &self.name,
                )
                .await
                .map_err(HarnessError::from)?;
            Some(entry)
        } else {
            None
        };
        if let Some(label) = options.label.as_deref() {
            let label_target = summary_entry
                .as_ref()
                .map(Entry::id)
                .or(target_id)
                .ok_or_else(|| HarnessError::fault("navigation label has no target"))?;
            self.session
                .lock()
                .await
                .set_label(label_target, Some(label))
                .await
                .map_err(HarnessError::from)?;
        }
        self.refresh_agent_context().await?;
        self.finish_operation_record(&run_id, "completed", None)
            .await?;
        self.release_operation(&run_id);
        Ok(NavigationResultValue {
            run_id,
            outcome: NavigationOutcome::Completed {
                new_leaf_id: summary_entry
                    .as_ref()
                    .map(|entry| entry.id().to_string())
                    .or_else(|| target_id.map(str::to_string)),
                summary_entry,
            },
        })
    }

    async fn resume(&self) -> Result<ResumeOutcome, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let existing_operation = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .cloned();
        let operation = if let Some(operation) = existing_operation {
            operation
        } else {
            let operation = self
                .session
                .lock()
                .await
                .find_open_operations(&self.name, Some(1))
                .await
                .map_err(HarnessError::from)?
                .into_iter()
                .next();
            let Some(LaneRecord::OperationStarted { id, intent, .. }) = operation else {
                return Err(HarnessError::nothing_to_resume(&self.name));
            };
            let operation = OperationInfo {
                id,
                kind: operation_kind(&intent),
                status: OperationStatus::Suspended,
            };
            self.active_operations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(self.name.clone(), operation.clone());
            operation
        };
        if operation.status == OperationStatus::Running {
            return Err(HarnessError::lane_busy(
                &self.name,
                &operation.id,
                operation.kind.as_str(),
            ));
        }
        if operation.status == OperationStatus::Aborting {
            return Err(HarnessError::fault(format!(
                "cannot resume aborting operation {}",
                operation.id
            )));
        }
        let records = self
            .session
            .lock()
            .await
            .find_records(&RecordQuery {
                order: Some(EntryOrder::OldestFirst),
                run_id: Some(operation.id.clone()),
                ..Default::default()
            })
            .await
            .map_err(HarnessError::from)?;
        let Some(LaneRecord::OperationStarted { intent, .. }) = records
            .iter()
            .find(|record| matches!(record, LaneRecord::OperationStarted { .. }))
        else {
            self.active_operations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.name);
            return Err(HarnessError::fault(format!(
                "open operation {} has no operation_started record",
                operation.id
            )));
        };
        match intent.clone() {
            OperationIntent::Run {
                original_prompt, ..
            } => {
                if self.agent.is_none() {
                    return self.runtime_required("resume");
                }
                let (run_id, messages, _) = self
                    .run_prompt_with_events_for_existing_operation(
                        original_prompt.clone(),
                        operation.id.clone(),
                    )
                    .await?;
                let result = self.run_result_from_messages(run_id, messages).await?;
                Ok(ResumeOutcome::Run {
                    run_id: operation.id,
                    outcome: result.outcome,
                })
            }
            OperationIntent::Compaction {
                custom_instructions,
                ..
            } => {
                let result = self.compact(custom_instructions.as_deref()).await?;
                Ok(ResumeOutcome::Compaction {
                    run_id: result.run_id,
                    outcome: result.outcome,
                })
            }
            OperationIntent::Navigation {
                target_id,
                summarize,
                custom_instructions,
                label,
                ..
            } => {
                let result = self
                    .navigate_tree(
                        target_id.as_deref(),
                        Some(&NavigateOptions {
                            summarize,
                            custom_instructions,
                            label,
                        }),
                    )
                    .await?;
                Ok(ResumeOutcome::Navigation {
                    run_id: result.run_id,
                    outcome: result.outcome,
                })
            }
        }
    }

    async fn abort(&self) -> Result<AbortResultValue, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let operation = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .cloned()
            .ok_or_else(|| HarnessError::no_active_operation(&self.name))?;

        // Record the durable intent before signalling the in-memory Agent, so
        // a crash after this point leaves an auditable abort request.
        self.session
            .lock()
            .await
            .append_record(NewRecord::AbortRequested {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                run_id: operation.id.clone(),
            })
            .await
            .map_err(HarnessError::from)?;

        self.set_operation_status(OperationStatus::Aborting);
        if let Some(signal) = self
            .operation_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&operation.id)
            .cloned()
        {
            signal.store(true, Ordering::SeqCst);
        }
        let steer = self
            .mark_queued_items("steer", QueueItemStatus::Cleared)
            .into_iter()
            .map(|item| item.message)
            .collect();
        let follow_up = self
            .mark_queued_items("followUp", QueueItemStatus::Cleared)
            .into_iter()
            .map(|item| item.message)
            .collect();
        if let Some(agent) = &self.agent {
            agent.clear_all_queues();
            agent.abort();
        } else {
            // A restored operation has no in-process Agent to signal. Its
            // durable abort request is still terminal and can be reconciled
            // immediately without pretending a provider call occurred.
            self.finish_operation_record(
                &operation.id,
                "aborted",
                Some(crate::session::types::OperationError {
                    code: "aborted".to_string(),
                    message: "operation aborted before runtime restoration".to_string(),
                }),
            )
            .await?;
            self.release_operation(&operation.id);
        }
        Ok(AbortResultValue {
            run_id: operation.id,
            steer,
            follow_up,
        })
    }

    async fn steer_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<String, HarnessError> {
        self.enqueue_message("steer", &Self::prompt_with_images(text, images))
            .await
    }

    async fn steer_message(&self, message: &AgentMessage) -> Result<String, HarnessError> {
        self.enqueue_message("steer", message).await
    }

    async fn follow_up_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<String, HarnessError> {
        self.enqueue_message("followUp", &Self::prompt_with_images(text, images))
            .await
    }

    async fn follow_up_message(&self, message: &AgentMessage) -> Result<String, HarnessError> {
        self.enqueue_message("followUp", message).await
    }

    async fn next_run_text(
        &self,
        text: &str,
        images: &[ImageContent],
    ) -> Result<String, HarnessError> {
        self.enqueue_message("nextRun", &Self::prompt_with_images(text, images))
            .await
    }

    async fn next_run_message(&self, message: &AgentMessage) -> Result<String, HarnessError> {
        self.enqueue_message("nextRun", message).await
    }

    async fn cancel_queued(&self, entry_id: &str) -> Result<CancelQueuedOutcome, HarnessError> {
        self.cancel_queue_item(entry_id).await
    }

    async fn record_usage(
        &self,
        usage: &Usage,
        options: Option<&RecordUsageOptions>,
    ) -> Result<(), HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let options = options.cloned().unwrap_or_default();
        let run_id = self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .map(|operation| operation.id.clone());
        self.session
            .lock()
            .await
            .append_record(NewRecord::Usage {
                id: crate::session::new_id(),
                lane: self.name.clone(),
                cause: "manual".to_string(),
                run_id,
                entry_id: options.entry_id,
                attempt: None,
                stop_reason: None,
                tool_call_id: None,
                details: options.details,
                usage: usage.clone(),
            })
            .await
            .map_err(HarnessError::from)?;
        Ok(())
    }

    async fn wait_for_idle(&self) -> Result<(), HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        if let Some(agent) = self.agent.clone() {
            agent.wait_for_idle().await;
        }
        // The harness operation registry is updated around the Agent call. A
        // caller may observe the Agent becoming idle one poll before the
        // harness has emitted its terminal lifecycle event, so wait for both
        // state machines to settle.
        while self
            .active_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .is_some_and(|operation| {
                matches!(
                    operation.status,
                    OperationStatus::Running | OperationStatus::Aborting
                )
            })
        {
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    async fn run_when_idle(&self, callback: RunWhenIdleCallback) -> Result<(), HarnessError> {
        self.wait_for_idle().await?;
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        callback().await;
        Ok(())
    }

    async fn peek_action(&self) -> Result<Option<ActionInfo>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        Ok(self
            .pending_actions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.name)
            .and_then(|actions| actions.front().cloned()))
    }

    async fn execute_action(&self) -> Result<Option<ActionInfo>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let action = self.peek_action().await?;
        let Some(action) = action.clone() else {
            return Ok(None);
        };
        // Agent and structural operations are already executing in their
        // spawned caller. Executing the visible action therefore means
        // driving that real operation to its durable terminal transition.
        self.wait_for_idle().await?;
        let empty = {
            let mut pending_actions = self
                .pending_actions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(actions) = pending_actions.get_mut(&self.name) {
                actions.pop_front();
                actions.is_empty()
            } else {
                true
            }
        };
        if empty {
            self.pending_actions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.name);
        }
        Ok(Some(action))
    }

    async fn run_to_completion(&self) -> Result<(), HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        loop {
            let action = self.peek_action().await?;
            if action.is_none() {
                self.wait_for_idle().await?;
                if self.peek_action().await?.is_none() {
                    return Ok(());
                }
            } else {
                self.execute_action().await?;
            }
        }
    }

    async fn get_model(&self) -> Model {
        self.model.clone()
    }

    async fn set_model(&mut self, model: Model) {
        self.model = model.clone();
        if let Some(agent) = &self.agent {
            agent.state().model = model;
        }
    }

    async fn get_thinking_level(&self) -> ModelThinkingLevel {
        self.thinking_level
    }

    async fn set_thinking_level(&mut self, level: ModelThinkingLevel) {
        self.thinking_level = level;
        if let Some(agent) = &self.agent {
            agent.state().thinking_level = reasoning_level(level);
        }
        if let Some(agent) = self.agent.as_mut().and_then(Arc::get_mut) {
            agent.set_reasoning(reasoning_level(level));
        }
    }

    async fn get_active_tools(&self) -> Vec<String> {
        self.active_tool_names.clone()
    }

    async fn set_active_tools(&mut self, names: Vec<String>) {
        self.active_tool_names = names;
        if let Some(agent) = &self.agent {
            let tools: Vec<AgentTool> = if self.active_tool_names.is_empty() {
                self.tools.iter().map(HarnessTool::to_agent_tool).collect()
            } else {
                self.tools
                    .iter()
                    .filter(|tool| {
                        self.active_tool_names
                            .iter()
                            .any(|name| name == tool.name())
                    })
                    .map(HarnessTool::to_agent_tool)
                    .collect()
            };
            agent.state().set_tools(tools);
        }
    }

    async fn get_tools(&self) -> Vec<HarnessTool> {
        self.tools.clone()
    }

    async fn set_tools(&mut self, tools: Vec<HarnessTool>, active_names: Option<Vec<String>>) {
        self.tools = tools.clone();
        self.active_tool_names =
            active_names.unwrap_or_else(|| tools.iter().map(|t| t.name().to_string()).collect());
        if let Some(agent) = &self.agent {
            let configured_tools: Vec<AgentTool> = if self.active_tool_names.is_empty() {
                self.tools.iter().map(HarnessTool::to_agent_tool).collect()
            } else {
                self.tools
                    .iter()
                    .filter(|tool| {
                        self.active_tool_names
                            .iter()
                            .any(|name| name == tool.name())
                    })
                    .map(HarnessTool::to_agent_tool)
                    .collect()
            };
            agent.state().set_tools(configured_tools);
        }
    }

    async fn get_resources(&self) -> Resources {
        self.resources.clone()
    }

    async fn set_resources(&mut self, resources: Resources) {
        self.resources = resources;
    }

    async fn get_stream_options(&self) -> StreamOptions {
        self.stream_options.clone()
    }

    async fn set_stream_options(&mut self, options: StreamOptions) {
        let mut options = options;
        if let Some(provider_context) = self.telemetry_context.provider_context() {
            options.base.base.telemetry_context = Some(provider_context);
        }
        self.stream_options = options.clone();
        if let Some(agent) = &self.agent {
            agent.set_stream_options(options);
        }
    }

    async fn get_retry_policy(&self) -> RetryPolicy {
        self.retry_policy.clone()
    }

    async fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry_policy = policy.clone();
        if let Some(agent) = &self.agent {
            agent.set_retry_policy(Some(policy));
        }
    }

    async fn get_compaction_settings(&self) -> CompactionSettings {
        self.compaction_settings.clone()
    }

    async fn set_compaction_settings(&mut self, settings: CompactionSettings) {
        self.compaction_settings = settings;
    }

    async fn get_steering_mode(&self) -> QueueMode {
        self.steering_mode
    }

    async fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering_mode = mode;
        if let Some(agent) = &self.agent {
            agent.set_steering_mode(match mode {
                QueueMode::All => crate::rich_agent::QueueMode::All,
                QueueMode::OneAtATime => crate::rich_agent::QueueMode::OneAtATime,
            });
        }
    }

    async fn get_follow_up_mode(&self) -> QueueMode {
        self.follow_up_mode
    }

    async fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
        if let Some(agent) = &self.agent {
            agent.set_follow_up_mode(match mode {
                QueueMode::All => crate::rich_agent::QueueMode::All,
                QueueMode::OneAtATime => crate::rich_agent::QueueMode::OneAtATime,
            });
        }
    }

    async fn watch(&self) -> Result<WatchHandle<LaneSnapshot>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let snapshot = LaneSnapshot {
            lane: self.name.clone(),
            transcript: self.transcript().await?,
            leaf_id: self.lane_leaf_id().await.map_err(HarnessError::from)?,
            operation: self
                .active_operations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&self.name)
                .cloned(),
            queues: self
                .queue_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&self.name)
                .cloned()
                .unwrap_or_default(),
            pending_writes: Vec::new(),
            faulted: false,
        };
        let event_watch = self
            .event_bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .watch(());
        Ok(WatchHandle::new(
            snapshot,
            Some(Box::new(move || event_watch.unsubscribe())),
        ))
    }

    async fn watch_session(&self) -> Result<WatchHandle<SessionSnapshot>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let lanes = self
            .lanes()
            .await?
            .into_iter()
            .map(|info| LaneInfoWithSuspended {
                info,
                suspended: None,
            })
            .collect();
        let event_watch = self
            .event_bus
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .watch(());
        Ok(WatchHandle::new(
            SessionSnapshot {
                lanes,
                faulted: false,
            },
            Some(Box::new(move || event_watch.unsubscribe())),
        ))
    }

    async fn close(&mut self) {
        if let Ok(mut closed) = self.closed.write() {
            *closed = true;
        }
        for signal in self
            .operation_signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            signal.store(true, Ordering::SeqCst);
        }
        if let Some(agent) = &self.agent {
            agent.clear_all_queues();
            agent.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Harness tests (ported from the pinned agent-harness oracle)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::fs::MemoryFs;
    use crate::session::memory::{in_memory_metadata, InMemorySessionStorage};
    use crate::session::types::{NewRecord, OperationIntent};
    use pi_ai::providers::{
        faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
        RegisterFauxProviderOptions,
    };
    use pi_ai::types::{
        AssistantMessageEvent, ContentBlock, Cost, DoneReason, Message, UserContent,
    };
    use pi_ai::AssistantMessageEventStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    type MemSession = Session<MemoryFs>;

    fn create_session(id: &str) -> MemSession {
        let storage = std::sync::Arc::new(std::sync::Mutex::new(InMemorySessionStorage::new(
            in_memory_metadata(id, None),
        )));
        Session::from_in_memory(storage)
    }

    fn test_model() -> Model {
        Model::new("gemini-2.5-flash", "Gemini 2.5 Flash", "google", "google")
    }

    async fn create_harness() -> AgentHarness<MemoryFs> {
        AgentHarness::create(AgentHarnessOptions::new(
            create_session("session"),
            test_model(),
        ))
        .await
        .unwrap()
        .0
    }

    #[test]
    fn session_id_is_forwarded_to_provider_stream_options_unless_explicit() {
        rt().block_on(async {
            let (implicit, _) = AgentHarness::create(AgentHarnessOptions::new(
                create_session("cache-affinity"),
                test_model(),
            ))
            .await
            .unwrap();
            assert_eq!(
                implicit
                    .get_stream_options()
                    .await
                    .base
                    .session_id
                    .as_deref(),
                Some("cache-affinity")
            );

            let mut explicit_options =
                AgentHarnessOptions::new(create_session("durable-session"), test_model());
            explicit_options.stream_options = Some(StreamOptions {
                base: pi_ai::types::StreamOptions {
                    session_id: Some("request-override".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            });
            let (explicit, _) = AgentHarness::create(explicit_options).await.unwrap();
            assert_eq!(
                explicit
                    .get_stream_options()
                    .await
                    .base
                    .session_id
                    .as_deref(),
                Some("request-override")
            );
        });
    }

    #[test]
    fn session_affinity_reaches_faux_cache_and_retention_none_opts_out() {
        rt().block_on(async {
            async fn run_two_turns(cache_retention: Option<&str>) -> (Usage, Usage) {
                let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
                core.set_responses(vec![
                    FauxResponseStep::Message(faux_assistant_message(
                        vec![ContentBlock::text("first reply")],
                        FauxAssistantOptions::default(),
                    )),
                    FauxResponseStep::Message(faux_assistant_message(
                        vec![ContentBlock::text("second reply")],
                        FauxAssistantOptions::default(),
                    )),
                ]);
                let legacy_core = core.clone();
                let stream_fn: crate::agent::StreamFn =
                    Arc::new(move |model, context| legacy_core.stream(model, context, None));
                let option_core = core.clone();
                let stream_fn_with_options: crate::agent::StreamFnWithOptions =
                    Arc::new(move |model, context, options| {
                        option_core.stream(model, context, Some(options))
                    });
                let mut options =
                    AgentHarnessOptions::new(create_session("cache-session"), test_model());
                options.stream_fn = Some(stream_fn);
                options.stream_fn_with_options = Some(stream_fn_with_options);
                options.stream_options = Some(StreamOptions {
                    base: pi_ai::types::StreamOptions {
                        cache_retention: cache_retention.map(str::to_string),
                        ..Default::default()
                    },
                    ..Default::default()
                });
                let (harness, _) = AgentHarness::create(options).await.unwrap();

                let first = harness
                    .run_prompt(vec![user_message("first")])
                    .await
                    .unwrap();
                let second = harness
                    .run_prompt(vec![user_message("second")])
                    .await
                    .unwrap();
                let assistant_usage = |messages: &[AgentMessage]| {
                    messages
                        .iter()
                        .find_map(|message| match message {
                            AgentMessage::Core(Message::Assistant(assistant)) => assistant.usage(),
                            _ => None,
                        })
                        .cloned()
                        .unwrap()
                };
                (assistant_usage(&first), assistant_usage(&second))
            }

            let (first, second) = run_two_turns(None).await;
            assert!(first.cache_write > 0);
            assert!(second.cache_read > 0);

            let (first, second) = run_two_turns(Some("none")).await;
            assert_eq!((first.cache_read, first.cache_write), (0, 0));
            assert_eq!((second.cache_read, second.cache_write), (0, 0));
        });
    }

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::Core(Message::User(UserContent::string(text, 1)))
    }

    fn usage() -> Usage {
        Usage {
            input: 1,
            output: 2,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 3,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.0,
            },
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn configured_harness_runs_agent_and_persists_lane_messages() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("reply")],
                FauxAssistantOptions::default(),
            ))]);
            let stream_fn: crate::agent::StreamFn =
                std::sync::Arc::new(move |model, context| core.stream(model, context, None));
            let mut options = AgentHarnessOptions::new(create_session("running"), test_model());
            options.stream_fn = Some(stream_fn);
            options.system_prompt = Some("system".into());
            let telemetry = Arc::new(InMemoryTelemetryContext::new());
            options.context = Some(HarnessTelemetryContext::InMemory(telemetry.clone()));
            let (mut harness, suspended) = AgentHarness::create(options).await.unwrap();
            assert!(suspended.is_empty());

            let lifecycle = Arc::new(Mutex::new(Vec::<String>::new()));
            let start_lifecycle = lifecycle.clone();
            harness.subscribe_event(
                "run_start",
                Box::new(move |event| {
                    start_lifecycle
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(event.event_type().to_string());
                }),
            );
            let end_lifecycle = lifecycle.clone();
            harness.subscribe_event(
                "run_end",
                Box::new(move |event| {
                    let outcome = event.as_run_end().expect("run_end event").outcome.as_str();
                    end_lifecycle
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(format!("{}:{outcome}", event.event_type()));
                }),
            );

            let messages = harness
                .run_prompt(vec![user_message("synthetic-secret-prompt")])
                .await
                .unwrap();
            assert_eq!(messages.len(), 2);
            assert_eq!(harness.agent_messages().await.unwrap().len(), 2);

            let transcript = harness.transcript().await.unwrap();
            assert_eq!(transcript.len(), 2);
            assert_eq!(transcript[0].as_message().unwrap(), &messages[0]);
            assert_eq!(transcript[1].as_message().unwrap(), &messages[1]);
            assert_eq!(
                *lifecycle.lock().unwrap_or_else(|error| error.into_inner()),
                vec!["run_start".to_string(), "run_end:completed".to_string()]
            );

            let spans = telemetry.get_spans();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].name, "pi.harness.run");
            assert_eq!(spans[0].attributes["pi.lane.name"], "main");
            assert_eq!(spans[0].attributes["pi.operation.kind"], "run");
            assert_eq!(spans[0].status, SpanStatus::Ok);
            assert!(spans[0].settled);
            assert_eq!(
                spans[0]
                    .events
                    .iter()
                    .map(|event| event.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["run_start", "run_end"]
            );
            let telemetry_json = serde_json::to_string(&spans[0].attributes).unwrap();
            assert!(!telemetry_json.contains("synthetic-secret-prompt"));
            assert!(!telemetry_json.contains("api_key"));
        });
    }

    #[test]
    fn terminal_provider_error_marks_lifecycle_failed() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("provider failed")],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::Error),
                    error_message: Some("overloaded".to_string()),
                },
            ))]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));
            let telemetry = Arc::new(InMemoryTelemetryContext::new());
            let mut options =
                AgentHarnessOptions::new(create_session("provider-error"), test_model());
            options.stream_fn = Some(stream_fn);
            options.context = Some(HarnessTelemetryContext::InMemory(telemetry.clone()));
            let (mut harness, _) = AgentHarness::create(options).await.unwrap();
            let lifecycle = Arc::new(Mutex::new(Vec::new()));
            let seen = lifecycle.clone();
            harness.subscribe_event(
                "run_end",
                Box::new(move |event| {
                    seen.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(event.as_run_end().unwrap().outcome);
                }),
            );

            let messages = harness
                .run_prompt(vec![user_message("hello")])
                .await
                .unwrap();

            assert!(messages.iter().any(|message| {
                matches!(
                    message,
                    AgentMessage::Core(Message::Assistant(assistant))
                        if assistant.stop_reason() == Some(pi_ai::types::StopReason::Error)
                )
            }));
            assert_eq!(
                *lifecycle.lock().unwrap_or_else(|error| error.into_inner()),
                vec![EventOutcome::Failed]
            );
            let spans = telemetry.get_spans();
            assert_eq!(spans.len(), 1);
            assert_eq!(
                spans[0].status,
                SpanStatus::Error {
                    error: Some(SpanError {
                        name: "ProviderError".to_string(),
                        message: "overloaded".to_string(),
                    })
                }
            );
            assert_eq!(spans[0].attributes["pi.operation.outcome"], "failed");
            assert_eq!(
                spans[0].events.last().map(|event| event.name.as_str()),
                Some("run_end")
            );
            assert_eq!(
                spans[0].events.last().unwrap().attributes["pi.operation.outcome"],
                "failed"
            );
        });
    }

    #[test]
    fn configured_resources_are_real_prompt_turns_with_exact_lookup_errors() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("skill reply")],
                    FauxAssistantOptions::default(),
                )),
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("template reply")],
                    FauxAssistantOptions::default(),
                )),
            ]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));
            let mut options =
                AgentHarnessOptions::new(create_session("resource-prompts"), test_model());
            options.stream_fn = Some(stream_fn);
            options.resources = Some(Resources {
                skills: vec![crate::types::Skill {
                    name: "build".to_string(),
                    description: "build things".to_string(),
                    content: "Run the build.".to_string(),
                    file_path: "/skills/build/SKILL.md".to_string(),
                    disable_model_invocation: false,
                }],
                prompt_templates: vec![crate::types::PromptTemplate {
                    name: "fix".to_string(),
                    description: Some("fix a target".to_string()),
                    content: "Fix $1 in $2".to_string(),
                }],
            });
            let (harness, _) = AgentHarness::create(options).await.unwrap();

            let skill_result = harness.skill("build", Some("Check the tests.")).await.unwrap();
            assert!(matches!(
                &skill_result.outcome,
                RunOutcome::Completed { .. }
            ));
            let args = ["parser".to_string(), "tests".to_string()];
            let template_result = harness
                .prompt_from_template("fix", Some(&args))
                .await
                .unwrap();
            assert!(matches!(
                template_result.outcome,
                RunOutcome::Completed { .. }
            ));

            let user_prompts = harness
                .transcript()
                .await
                .unwrap()
                .iter()
                .filter_map(|entry| {
                    let message = entry.as_message()?;
                    match message {
                        AgentMessage::Core(Message::User(user)) => {
                            Some(crate::agent::user_content_text(user))
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(
                user_prompts,
                vec![
                    "<skill name=\"build\" location=\"/skills/build/SKILL.md\">\nReferences are relative to /skills/build.\n\nRun the build.\n</skill>\n\nCheck the tests.".to_string(),
                    "Fix parser in tests".to_string(),
                ]
            );
            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(records.iter().any(|record| matches!(
                record,
                crate::session::types::LaneRecord::OperationStarted { id, .. }
                    if id == &skill_result.run_id
            )));
            assert!(records.iter().any(|record| matches!(
                record,
                crate::session::types::LaneRecord::OperationFinished {
                    run_id,
                    outcome,
                    ..
                } if run_id == &skill_result.run_id && outcome == "completed"
            )));

            let callback_called = Arc::new(AtomicBool::new(false));
            let callback_flag = callback_called.clone();
            harness
                .run_when_idle(Arc::new(move || {
                    let callback_flag = callback_flag.clone();
                    Box::pin(async move {
                        callback_flag.store(true, Ordering::SeqCst);
                    })
                }))
                .await
                .unwrap();
            assert!(callback_called.load(Ordering::SeqCst));

            let unknown_skill = harness.skill("missing", None).await.unwrap_err();
            assert_eq!(
                match unknown_skill {
                    HarnessError::Tagged(tag) => tag.to_json(),
                    other => panic!("expected UnknownSkill, got {other:?}"),
                },
                serde_json::json!({
                    "_tag": "UnknownSkill",
                    "message": "unknown skill missing",
                    "name": "missing",
                })
            );
            let unknown_template = harness
                .prompt_from_template("missing", None)
                .await
                .unwrap_err();
            assert_eq!(
                match unknown_template {
                    HarnessError::Tagged(tag) => tag.to_json(),
                    other => panic!("expected UnknownTemplate, got {other:?}"),
                },
                serde_json::json!({
                    "_tag": "UnknownTemplate",
                    "message": "unknown prompt template missing",
                    "name": "missing",
                })
            );
        });
    }

    #[test]
    fn manual_compaction_calls_the_provider_persists_summary_and_retains_tail() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("durable summary")],
                FauxAssistantOptions::default(),
            ))]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));

            let mut session = create_session("manual-compaction");
            for (id, message) in [
                ("old-user", user_message("old request")),
                ("old-assistant", user_message("old answer")),
                ("retained-user", user_message("keep this tail")),
            ] {
                session
                    .append_entry(
                        EntryNoStats::Message {
                            id: id.to_string(),
                            message,
                            terminate: None,
                        },
                        "main",
                    )
                    .await
                    .unwrap();
            }
            let mut options = AgentHarnessOptions::new(session, test_model());
            options.stream_fn = Some(stream_fn);
            options.compaction = Some(CompactionSettings {
                enabled: true,
                reserve_tokens: 16,
                keep_recent_tokens: 1,
            });
            let (harness, suspended) = AgentHarness::create(options).await.unwrap();
            assert!(suspended.is_empty());

            let result = harness.compact(Some("focus on decisions")).await.unwrap();
            let run_id = result.run_id.clone();
            let CompactionOutcome::Completed { entry, .. } = result.outcome else {
                panic!("manual compaction should complete")
            };
            let Entry::Compaction {
                summary,
                retained_tail,
                tokens_before,
                ..
            } = entry
            else {
                panic!("compaction must persist a compaction entry")
            };
            assert!(summary.contains("durable summary"));
            assert_eq!(retained_tail.len(), 1);
            assert!(tokens_before > 0);
            assert!(matches!(
                &retained_tail[0],
                AgentMessage::Core(Message::User(user))
                    if crate::agent::user_content_text(user) == "keep this tail"
            ));
            let context_messages = harness.agent_messages().await.unwrap();
            assert_eq!(context_messages.len(), 2);
            assert_eq!(context_messages[0].role(), "compactionSummary");
            assert_eq!(context_messages[1].role(), "user");

            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    run_id: Some(run_id.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(matches!(
                records.first(),
                Some(LaneRecord::OperationStarted {
                    id,
                    intent: OperationIntent::Compaction {
                        custom_instructions: Some(instructions),
                        ..
                    },
                    ..
                }) if id == &run_id && instructions == "focus on decisions"
            ));
            assert!(records.iter().any(|record| matches!(
                record,
                LaneRecord::StepAttempt {
                    run_id: record_run_id,
                    step,
                    compaction_reason: Some(reason),
                    ..
                } if record_run_id == &run_id && step == "compaction" && reason == "manual"
            )));
            assert!(records.iter().any(|record| matches!(
                record,
                LaneRecord::OperationFinished {
                    run_id: record_run_id,
                    outcome,
                    error: None,
                    ..
                } if record_run_id == &run_id && outcome == "completed"
            )));
        });
    }

    #[test]
    fn navigation_moves_to_target_and_persists_a_real_branch_summary_and_label() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("branch summary")],
                FauxAssistantOptions::default(),
            ))]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));

            let mut session = create_session("navigation");
            session
                .append_entry(
                    EntryNoStats::Message {
                        id: "root-user".to_string(),
                        message: user_message("root request"),
                        terminate: None,
                    },
                    "main",
                )
                .await
                .unwrap();
            session
                .append_entry(
                    EntryNoStats::Message {
                        id: "branch-user".to_string(),
                        message: user_message("explored branch"),
                        terminate: None,
                    },
                    "main",
                )
                .await
                .unwrap();
            let mut options = AgentHarnessOptions::new(session, test_model());
            options.stream_fn = Some(stream_fn);
            let (harness, suspended) = AgentHarness::create(options).await.unwrap();
            assert!(suspended.is_empty());

            let result = harness
                .navigate_tree(
                    Some("root-user"),
                    Some(&NavigateOptions {
                        summarize: true,
                        custom_instructions: Some("preserve decisions".to_string()),
                        label: Some("return point".to_string()),
                    }),
                )
                .await
                .unwrap();
            let run_id = result.run_id.clone();
            let NavigationOutcome::Completed {
                new_leaf_id: Some(new_leaf_id),
                summary_entry: Some(summary_entry),
            } = result.outcome
            else {
                panic!("summarized navigation should complete with a summary entry")
            };
            assert_eq!(new_leaf_id, summary_entry.id());
            assert!(matches!(
                summary_entry,
                Entry::BranchSummary {
                    summary,
                    from_id,
                    ..
                } if summary.contains("branch summary") && from_id == "branch-user"
            ));
            assert_eq!(
                harness.get_leaf_id().await.unwrap().as_deref(),
                Some(new_leaf_id.as_str())
            );
            let session = harness.session();
            let session = session.lock().await;
            assert_eq!(session.get_label(&new_leaf_id).await.as_deref(), Some("return point"));
            drop(session);

            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    run_id: Some(run_id.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(matches!(
                records.first(),
                Some(LaneRecord::OperationStarted {
                    id,
                    intent: OperationIntent::Navigation {
                        summarize: true,
                        custom_instructions: Some(instructions),
                        label: Some(label),
                        ..
                    },
                    ..
                }) if id == &run_id && instructions == "preserve decisions" && label == "return point"
            ));
            assert!(records.iter().any(|record| matches!(
                record,
                LaneRecord::OperationFinished {
                    run_id: record_run_id,
                    outcome,
                    error: None,
                    ..
                } if record_run_id == &run_id && outcome == "completed"
            )));
        });
    }

    #[test]
    fn resume_replays_a_durable_run_with_the_same_operation_id_and_finishes_it() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("resumed reply")],
                FauxAssistantOptions::default(),
            ))]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));

            let mut session = create_session("resume");
            session
                .append_record(NewRecord::OperationStarted {
                    id: "resume-op".to_string(),
                    lane: "main".to_string(),
                    source_leaf_id: None,
                    intent: OperationIntent::Run {
                        original_prompt: vec![user_message("resume this")],
                        initial_messages: Vec::new(),
                        system_prompt_override: None,
                        resume_data: None,
                    },
                })
                .await
                .unwrap();
            let mut options = AgentHarnessOptions::new(session, test_model());
            options.stream_fn = Some(stream_fn);
            let (harness, suspended) = AgentHarness::create(options).await.unwrap();
            assert_eq!(suspended.len(), 1);
            assert_eq!(suspended[0].prompt, Some(vec![user_message("resume this")]));

            let resumed = harness.resume().await.unwrap();
            let ResumeOutcome::Run { run_id, outcome } = resumed else {
                panic!("run operation should resume as a run")
            };
            assert_eq!(run_id, "resume-op");
            assert!(matches!(outcome, RunOutcome::Completed { .. }));
            assert!(harness.lanes().await.unwrap()[0].operation.is_none());

            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    run_id: Some("resume-op".to_string()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(
                records
                    .iter()
                    .filter(|record| matches!(record, LaneRecord::OperationStarted { .. }))
                    .count(),
                1
            );
            assert!(records.iter().any(|record| matches!(
                record,
                LaneRecord::OperationFinished { run_id, outcome, .. }
                    if run_id == "resume-op" && outcome == "completed"
            )));
            let transcript = harness.transcript().await.unwrap();
            assert_eq!(transcript.len(), 2);
            assert!(transcript[1].as_message().is_some_and(|message| {
                matches!(
                    message,
                    AgentMessage::Core(Message::Assistant(message))
                        if message.content().iter().any(|block| matches!(
                            block,
                            ContentBlock::Text { text, .. } if text == "resumed reply"
                        ))
                )
            }));
        });
    }

    #[test]
    fn public_hooks_and_events_dispatch_payloads_share_lanes_and_unsubscribe() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("first")],
                    FauxAssistantOptions::default(),
                )),
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("second")],
                    FauxAssistantOptions::default(),
                )),
            ]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));
            let mut options = AgentHarnessOptions::new(create_session("registry"), test_model());
            options.stream_fn = Some(stream_fn);
            let (harness, _) = AgentHarness::create(options).await.unwrap();

            let hooks_seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let hook_values = hooks_seen.clone();
            let unsubscribe_hook = harness
                .hooks
                .on(
                    HookName::BeforeRun.as_str(),
                    Arc::new(move |payload| {
                        hook_values
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(payload.clone())
                    }),
                )
                .unwrap();
            let events_seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let event_values = events_seen.clone();
            let unsubscribe_event = harness
                .events
                .on(
                    "run_start",
                    Arc::new(move |payload| {
                        event_values
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(payload.clone())
                    }),
                )
                .unwrap();
            let event_values = events_seen.clone();
            let unsubscribe_end = harness
                .events
                .on(
                    "run_end",
                    Arc::new(move |payload| {
                        event_values
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(payload.clone())
                    }),
                )
                .unwrap();
            assert_eq!(harness.hooks.listener_count("before_run"), 1);
            assert_eq!(harness.events.listener_count("run_start"), 1);
            assert_eq!(harness.events.listener_count("run_end"), 1);

            let lane = harness.lane("main").await.unwrap();
            lane.prompt_text("first prompt", &[]).await.unwrap();
            assert_eq!(
                hooks_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .len(),
                1
            );
            assert_eq!(
                events_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .len(),
                2
            );
            assert_eq!(
                hooks_seen.lock().unwrap_or_else(|error| error.into_inner())[0]["lane"],
                "main"
            );
            assert_eq!(
                events_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())[0]["type"],
                "run_start"
            );
            assert_eq!(
                events_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())[1]["type"],
                "run_end"
            );
            assert_eq!(
                events_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())[1]["outcome"],
                "completed"
            );
            assert!(events_seen
                .lock()
                .unwrap_or_else(|error| error.into_inner())[0]["runId"]
                .is_string());

            unsubscribe_hook();
            unsubscribe_event();
            unsubscribe_end();
            assert_eq!(harness.hooks.listener_count("before_run"), 0);
            assert_eq!(harness.events.listener_count("run_start"), 0);
            assert_eq!(harness.events.listener_count("run_end"), 0);
            harness.prompt_text("second prompt", &[]).await.unwrap();
            assert_eq!(
                hooks_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .len(),
                1
            );
            assert_eq!(
                events_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .len(),
                2
            );
        });
    }

    #[test]
    fn live_abort_is_durable_cancellable_and_lane_busy_is_exactly_tagged() {
        rt().block_on(async {
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let stream_fn: crate::agent::StreamFn = {
                let started = started.clone();
                let release = release.clone();
                Arc::new(move |_model, _context| {
                    let stream = AssistantMessageEventStream::new();
                    let sender = stream.sender().expect("stream sender");
                    let started = started.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        started.notify_one();
                        release.notified().await;
                        let _ = sender.send(AssistantMessageEvent::Done {
                            reason: DoneReason::Stop,
                            message: faux_assistant_message(
                                vec![ContentBlock::text("late response")],
                                FauxAssistantOptions::default(),
                            ),
                        });
                    });
                    stream
                })
            };
            let mut options = AgentHarnessOptions::new(create_session("abort"), test_model());
            options.stream_fn = Some(stream_fn);
            let (harness, _) = AgentHarness::create(options).await.unwrap();

            let runner = harness.lane("main").await.unwrap();
            let run_task = tokio::spawn(async move { runner.prompt_text("run", &[]).await });
            started.notified().await;

            let busy_lane = harness.lane("main").await.unwrap();
            let idle_lane = harness.lane("main").await.unwrap();
            let idle_task = tokio::spawn(async move { idle_lane.wait_for_idle().await });
            let busy_error = busy_lane.prompt_text("second", &[]).await.unwrap_err();
            let operation_id = match &busy_error {
                HarnessError::Tagged(tag) => {
                    assert_eq!(tag.tag, "LaneBusy");
                    tag.payload["operationId"].as_str().unwrap().to_string()
                }
                other => panic!("expected LaneBusy, got {other:?}"),
            };
            assert_eq!(
                match busy_error {
                    HarnessError::Tagged(tag) => tag.to_json(),
                    other => panic!("expected LaneBusy, got {other:?}"),
                },
                serde_json::json!({
                    "_tag": "LaneBusy",
                    "message": "lane main is busy",
                    "lane": "main",
                    "operationId": operation_id.clone(),
                    "operationKind": "run",
                })
            );

            let _queued = busy_lane
                .steer_message(&user_message("abort me later"))
                .await
                .unwrap();
            let abort = busy_lane.abort().await.unwrap();
            assert_eq!(abort.run_id, operation_id);
            assert_eq!(abort.follow_up, Vec::<AgentMessage>::new());
            assert_eq!(abort.steer.len(), 1);
            assert_eq!(
                match &abort.steer[0] {
                    AgentMessage::Core(Message::User(user)) =>
                        crate::agent::user_content_text(user),
                    other => panic!("expected queued user message, got {other:?}"),
                },
                "abort me later"
            );

            // Wake the producer as well; the Agent has already observed the
            // abort flag, so a raced provider completion remains aborted.
            release.notify_one();
            let run_result = tokio::time::timeout(std::time::Duration::from_secs(1), run_task)
                .await
                .expect("aborted run should settle")
                .unwrap()
                .unwrap();
            assert!(matches!(run_result.outcome, RunOutcome::Aborted { .. }));
            busy_lane.wait_for_idle().await.unwrap();
            idle_task.await.unwrap().unwrap();

            let no_active_operation = busy_lane.abort().await.unwrap_err();
            assert_eq!(
                match no_active_operation {
                    HarnessError::Tagged(tag) => tag.to_json(),
                    other => panic!("expected NoActiveOperation, got {other:?}"),
                },
                serde_json::json!({
                    "_tag": "NoActiveOperation",
                    "message": "no active operation in lane main",
                    "lane": "main",
                })
            );
            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(records.iter().any(|record| matches!(
                record,
                crate::session::types::LaneRecord::AbortRequested { run_id, .. }
                    if run_id == &operation_id
            )));
            assert!(busy_lane
                .watch()
                .await
                .unwrap()
                .snapshot
                .queues
                .steer
                .is_empty());
        });
    }

    #[test]
    fn closing_a_live_harness_signals_the_real_agent() {
        rt().block_on(async {
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let stream_fn: crate::agent::StreamFn = {
                let started = started.clone();
                let release = release.clone();
                Arc::new(move |_model, _context| {
                    let stream = AssistantMessageEventStream::new();
                    let sender = stream.sender().expect("stream sender");
                    let started = started.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        started.notify_one();
                        release.notified().await;
                        let _ = sender.send(AssistantMessageEvent::Done {
                            reason: DoneReason::Stop,
                            message: faux_assistant_message(
                                vec![ContentBlock::text("close race")],
                                FauxAssistantOptions::default(),
                            ),
                        });
                    });
                    stream
                })
            };
            let mut options = AgentHarnessOptions::new(create_session("close"), test_model());
            options.stream_fn = Some(stream_fn);
            let (mut harness, _) = AgentHarness::create(options).await.unwrap();
            let runner = harness.lane("main").await.unwrap();
            let run_task = tokio::spawn(async move { runner.prompt_text("run", &[]).await });
            started.notified().await;

            harness.close().await;
            release.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), run_task)
                .await
                .expect("close should not strand a live run")
                .unwrap()
                .unwrap();
            assert!(matches!(result.outcome, RunOutcome::Aborted { .. }));
            assert!(harness.is_closed());
            assert!(matches!(
                harness.prompt_text("after close", &[]).await,
                Err(HarnessError::Closed)
            ));
        });
    }

    #[test]
    fn active_lane_queues_persist_and_cancel_items() {
        rt().block_on(async {
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let stream_fn: crate::agent::StreamFn = {
                let started = started.clone();
                let release = release.clone();
                Arc::new(move |_model, _context| {
                    let stream = AssistantMessageEventStream::new();
                    let sender = stream.sender().expect("stream sender");
                    let started = started.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        started.notify_one();
                        release.notified().await;
                        let message = faux_assistant_message(
                            vec![ContentBlock::text("queued run")],
                            FauxAssistantOptions::default(),
                        );
                        let _ = sender.send(AssistantMessageEvent::Done {
                            reason: DoneReason::Stop,
                            message,
                        });
                    });
                    stream
                })
            };
            let mut options = AgentHarnessOptions::new(create_session("queue-state"), test_model());
            options.stream_fn = Some(stream_fn);
            let (harness, _) = AgentHarness::create(options).await.unwrap();

            let runner = harness.lane("main").await.unwrap();
            let run_task = tokio::spawn(async move {
                let prompts = vec![user_message("run")];
                runner.prompt_messages(&prompts).await
            });
            started.notified().await;

            let lane = harness.lane("main").await.unwrap();
            let queued_id = lane
                .steer_message(&user_message("steer while running"))
                .await
                .unwrap();
            let snapshot = lane.watch().await.unwrap().snapshot;
            assert_eq!(snapshot.queues.steer.len(), 1);
            assert_eq!(snapshot.queues.steer[0].entry_id, queued_id);
            assert_eq!(snapshot.operation.unwrap().status, OperationStatus::Running);
            assert!(harness.lanes().await.unwrap()[0].operation.is_some());

            assert_eq!(
                lane.cancel_queued(&queued_id).await.unwrap(),
                CancelQueuedOutcome::Cancelled,
            );
            assert!(lane.watch().await.unwrap().snapshot.queues.steer.is_empty());
            release.notify_one();
            run_task.await.unwrap().unwrap();

            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(records.iter().any(|record| matches!(
                record,
                crate::session::types::LaneRecord::QueueEnqueued { queue, .. }
                    if queue == "steer"
            )));
            assert!(records.iter().any(|record| matches!(
                record,
                crate::session::types::LaneRecord::QueueCancelled { entry_id, .. }
                    if entry_id == &queued_id
            )));
        });
    }

    #[test]
    fn record_usage_persists_usage_telemetry() {
        rt().block_on(async {
            let harness = create_harness().await;
            harness
                .record_usage(
                    &usage(),
                    Some(&RecordUsageOptions {
                        entry_id: Some("assistant-entry".to_string()),
                        details: Some(serde_json::json!({"source": "test"})),
                    }),
                )
                .await
                .unwrap();
            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    record_type: Some("usage".to_string()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(matches!(
                records.as_slice(),
                [crate::session::types::LaneRecord::Usage {
                    cause,
                    entry_id,
                    details: Some(details),
                    usage: recorded_usage,
                    ..
                }] if cause == "manual"
                    && entry_id == "assistant-entry"
                    && details["source"] == "test"
                    && recorded_usage == &usage()
            ));
        });
    }

    #[test]
    fn secondary_lane_has_branch_context_and_shared_lifecycle() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("main reply")],
                    FauxAssistantOptions::default(),
                )),
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("thread reply")],
                    FauxAssistantOptions::default(),
                )),
            ]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));
            let telemetry = Arc::new(InMemoryTelemetryContext::new());
            let mut options = AgentHarnessOptions::new(create_session("lanes"), test_model());
            options.stream_fn = Some(stream_fn);
            options.context = Some(HarnessTelemetryContext::InMemory(telemetry.clone()));
            let (mut harness, _) = AgentHarness::create(options).await.unwrap();

            let lifecycle = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = lifecycle.clone();
            harness.subscribe_event(
                "run_start",
                Box::new(move |event| {
                    seen.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(format!("start:{}", event.as_run_start().unwrap().lane));
                }),
            );
            let seen = lifecycle.clone();
            harness.subscribe_event(
                "run_end",
                Box::new(move |event| {
                    seen.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(format!(
                            "end:{}:{}",
                            event.as_run_end().unwrap().lane,
                            event.as_run_end().unwrap().outcome.as_str()
                        ));
                }),
            );

            harness
                .run_prompt(vec![user_message("main prompt")])
                .await
                .unwrap();
            let main_leaf = harness
                .session()
                .lock()
                .await
                .get_leaf_id()
                .await
                .unwrap()
                .unwrap();

            let thread = harness
                .create_lane("thread", Some(&main_leaf))
                .await
                .unwrap();
            assert_eq!(thread.lane_name(), "thread");
            assert_eq!(thread.get_leaf_id().await.unwrap(), Some(main_leaf.clone()));

            let result = thread
                .prompt_messages(&[user_message("thread prompt")])
                .await
                .unwrap();
            assert!(matches!(
                result.outcome,
                RunOutcome::Completed { ref leaf_id, .. } if !leaf_id.is_empty()
            ));

            let lanes = harness.lanes().await.unwrap();
            assert_eq!(
                lanes
                    .iter()
                    .map(|lane| lane.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["main", "thread"]
            );
            assert_ne!(
                lanes[0].leaf_id.as_deref(),
                lanes[1].leaf_id.as_deref(),
                "the thread run must advance only its own lane pointer"
            );

            let session_handle = harness.session();
            let mut session = session_handle.lock().await;
            let thread_entries = session
                .view("thread")
                .find_entries_on_branch(
                    &EntryQuery {
                        order: Some(EntryOrder::OldestFirst),
                        ..Default::default()
                    },
                    &BranchBounds::default(),
                )
                .await
                .unwrap();
            assert_eq!(thread_entries.len(), 4);
            assert_eq!(
                thread_entries
                    .iter()
                    .filter_map(|entry| entry.as_message())
                    .filter_map(|message| match message {
                        AgentMessage::Core(Message::User(user)) => {
                            Some(crate::agent::user_content_text(user))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec!["main prompt", "thread prompt"]
            );

            assert_eq!(
                *lifecycle.lock().unwrap_or_else(|error| error.into_inner()),
                vec![
                    "start:main",
                    "end:main:completed",
                    "start:thread",
                    "end:thread:completed"
                ]
            );
            let spans = telemetry.get_spans();
            assert_eq!(spans.len(), 2);
            assert_eq!(spans[0].attributes["pi.lane.name"], "main");
            assert_eq!(spans[1].attributes["pi.lane.name"], "thread");
        });
    }

    #[test]
    fn mode_lifecycle_adapter_preserves_event_and_span_order() {
        rt().block_on(async {
            let telemetry =
                HarnessTelemetryContext::InMemory(Arc::new(InMemoryTelemetryContext::new()));
            let mut bus = HarnessEventBus::new();
            let lifecycle = Arc::new(Mutex::new(Vec::<String>::new()));
            let start_lifecycle = lifecycle.clone();
            bus.on(
                "run_start",
                Box::new(move |_| {
                    start_lifecycle
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push("start".into())
                }),
            );
            let end_lifecycle = lifecycle.clone();
            bus.on(
                "run_end",
                Box::new(move |event| {
                    end_lifecycle
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(format!(
                            "end:{}",
                            event.as_run_end().unwrap().outcome.as_str()
                        ));
                }),
            );

            let value = run_with_harness_lifecycle(
                &telemetry,
                &mut bus,
                "main",
                "session",
                "leaf".into(),
                |span| async move {
                    span.add_event("inner", None);
                    Ok::<_, HarnessError>(7)
                },
            )
            .await
            .unwrap();
            assert_eq!(value, 7);
            assert_eq!(
                *lifecycle.lock().unwrap_or_else(|error| error.into_inner()),
                vec!["start".to_string(), "end:completed".to_string()]
            );

            let spans = match telemetry {
                HarnessTelemetryContext::InMemory(context) => context.get_spans(),
                HarnessTelemetryContext::Noop => unreachable!(),
            };
            assert_eq!(spans.len(), 1);
            assert_eq!(
                spans[0]
                    .events
                    .iter()
                    .map(|event| event.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["run_start", "inner", "run_end"]
            );
            assert_eq!(spans[0].attributes["pi.session.id"], "session");
            assert_eq!(spans[0].attributes["pi.operation.outcome"], "completed");
        });
    }

    #[test]
    fn restores_open_operations_for_resume_or_abort() {
        rt().block_on(async {
            let session = create_session("session");
            let (mut harness, suspended) =
                AgentHarness::create(AgentHarnessOptions::new(session, test_model()))
                    .await
                    .unwrap();
            assert!(suspended.is_empty());
            assert_eq!(harness.lane_name(), "main");
            assert_eq!(harness.get_leaf_id().await.unwrap(), None);
            assert_eq!(
                harness.session().lock().await.get_leaf_id().await.unwrap(),
                None
            );
            harness.close().await;

            let mut recorded = create_session("recorded");
            recorded
                .append_record(NewRecord::OperationStarted {
                    id: "op".to_string(),
                    lane: "main".to_string(),
                    source_leaf_id: None,
                    intent: OperationIntent::Run {
                        original_prompt: vec![],
                        initial_messages: vec![],
                        system_prompt_override: None,
                        resume_data: None,
                    },
                })
                .await
                .unwrap();
            let restored_steer = user_message("restored steer");
            recorded
                .append_record(NewRecord::QueueEnqueued {
                    id: "queue-steer-record".to_string(),
                    lane: "main".to_string(),
                    queue: "steer".to_string(),
                    run_id: "op".to_string(),
                    target: serde_json::to_value(EntryNoStats::Message {
                        id: "restored-steer".to_string(),
                        message: restored_steer.clone(),
                        terminate: None,
                    })
                    .unwrap(),
                })
                .await
                .unwrap();
            let restored_next_run = user_message("restored next run");
            recorded
                .append_record(NewRecord::QueueEnqueued {
                    id: "queue-next-record".to_string(),
                    lane: "main".to_string(),
                    queue: "nextRun".to_string(),
                    run_id: "op".to_string(),
                    target: serde_json::to_value(EntryNoStats::Message {
                        id: "restored-next-run".to_string(),
                        message: restored_next_run.clone(),
                        terminate: None,
                    })
                    .unwrap(),
                })
                .await
                .unwrap();
            let (harness, suspended) = AgentHarness::create(AgentHarnessOptions::new(
                recorded,
                test_model(),
            ))
                .await
                .unwrap();
            assert_eq!(suspended.len(), 1);
            assert_eq!(suspended[0].id, "op");
            assert_eq!(suspended[0].kind, OperationKind::Run);
            assert_eq!(suspended[0].reason, SuspensionReason::Crash);
            assert!(matches!(
                harness.lanes().await.unwrap().as_slice(),
                [LaneInfo {
                    operation: Some(OperationInfo {
                        id,
                        kind: OperationKind::Run,
                        status: OperationStatus::Suspended,
                    }),
                    ..
                }] if id == "op"
            ));
            let restored_snapshot = harness.watch().await.unwrap().snapshot;
            assert_eq!(
                restored_snapshot.queues.steer,
                vec![QueuedItem {
                    entry_id: "restored-steer".to_string(),
                    message: restored_steer,
                }]
            );
            assert_eq!(
                restored_snapshot.queues.next_run,
                vec![QueuedItem {
                    entry_id: "restored-next-run".to_string(),
                    message: restored_next_run,
                }]
            );
            assert!(matches!(
                harness.resume().await,
                Err(HarnessError::Fault { message })
                    if message == "AgentHarness.resume requires a configured stream function"
            ));
            let abort = harness.abort().await.unwrap();
            assert_eq!(abort.run_id, "op");
            assert_eq!(abort.steer.len(), 1);
            assert!(matches!(
                &abort.steer[0],
                AgentMessage::Core(Message::User(user))
                    if crate::agent::user_content_text(user) == "restored steer"
            ));
            let after_abort = harness.watch().await.unwrap().snapshot;
            assert!(after_abort.queues.steer.is_empty());
            assert_eq!(after_abort.queues.next_run.len(), 1);
            assert!(matches!(harness.resume().await, Err(HarnessError::Tagged(tag)) if tag.tag == "NothingToResume"));
            let records = harness
                .session()
                .lock()
                .await
                .find_records(&RecordQuery {
                    order: Some(EntryOrder::OldestFirst),
                    run_id: Some("op".to_string()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(records.iter().any(|record| matches!(
                record,
                LaneRecord::AbortRequested { run_id, .. } if run_id == "op"
            )));
            assert!(records.iter().any(|record| matches!(
                record,
                LaneRecord::OperationFinished { run_id, outcome, .. }
                    if run_id == "op" && outcome == "aborted"
            )));
        });
    }

    #[test]
    fn keeps_configuration_as_defensive_copies() {
        rt().block_on(async {
            let mut harness = create_harness().await;
            let model = Model::new(
                "claude-sonnet-4-5",
                "Claude Sonnet 4.5",
                "anthropic",
                "anthropic",
            );
            harness.set_model(model.clone()).await;
            assert_eq!(harness.get_model().await, model);

            harness.set_thinking_level(ModelThinkingLevel::High).await;
            assert_eq!(harness.get_thinking_level().await, ModelThinkingLevel::High);

            let mut active_tools = vec!["one".to_string()];
            harness.set_active_tools(active_tools.clone()).await;
            active_tools.push("mutated".to_string());
            assert_eq!(harness.get_active_tools().await, vec!["one"]);
            let mut read_active = harness.get_active_tools().await;
            read_active.push("mutated".to_string());
            assert_eq!(harness.get_active_tools().await, vec!["one"]);

            let tool = HarnessTool {
                tool: pi_ai::types::json_tool(
                    "tool",
                    "Tool",
                    &serde_json::json!({ "type": "object" }),
                ),
                execute: crate::tools::read_tool(".".to_string()).execute,
                prepare_arguments: None,
                execution_mode: None,
                replay: None,
            };
            let mut tools = vec![tool.clone()];
            harness.set_tools(tools.clone(), None).await;
            tools.push(HarnessTool {
                tool: pi_ai::types::json_tool(
                    "mutated",
                    "Mutated",
                    &serde_json::json!({ "type": "object" }),
                ),
                execute: crate::tools::read_tool(".".to_string()).execute,
                prepare_arguments: None,
                execution_mode: None,
                replay: None,
            });
            assert_eq!(
                harness
                    .get_tools()
                    .await
                    .iter()
                    .map(|t| t.name().to_string())
                    .collect::<Vec<_>>(),
                vec!["tool"]
            );

            let mut resources = Resources {
                skills: vec![crate::types::Skill {
                    name: "skill".to_string(),
                    description: "desc".to_string(),
                    content: "body".to_string(),
                    file_path: "/tmp/SKILL.md".to_string(),
                    disable_model_invocation: false,
                }],
                prompt_templates: vec![crate::types::PromptTemplate {
                    name: "template".to_string(),
                    description: None,
                    content: "body".to_string(),
                }],
            };
            harness.set_resources(resources.clone()).await;
            resources.skills.push(crate::types::Skill {
                name: "mutated".to_string(),
                description: "d".to_string(),
                content: "b".to_string(),
                file_path: "/tmp/OTHER.md".to_string(),
                disable_model_invocation: false,
            });
            assert_eq!(
                harness
                    .get_resources()
                    .await
                    .skills
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>(),
                vec!["skill"]
            );

            let mut stream_options = SimpleStreamOptions::default();
            stream_options.base.max_tokens = Some(10);
            harness.set_stream_options(stream_options.clone()).await;
            stream_options.base.max_tokens = Some(20);
            assert_eq!(harness.get_stream_options().await.base.max_tokens, Some(10));

            let retry_policy = RetryPolicy {
                enabled: true,
                max_retries: 2,
                base_delay_ms: 10,
            };
            harness.set_retry_policy(retry_policy).await;
            assert_eq!(
                harness.get_retry_policy().await,
                RetryPolicy {
                    enabled: true,
                    max_retries: 2,
                    base_delay_ms: 10
                }
            );

            let compaction_settings = CompactionSettings {
                enabled: false,
                reserve_tokens: 1,
                keep_recent_tokens: 2,
            };
            harness.set_compaction_settings(compaction_settings).await;
            assert_eq!(
                harness.get_compaction_settings().await,
                CompactionSettings {
                    enabled: false,
                    reserve_tokens: 1,
                    keep_recent_tokens: 2
                }
            );

            harness.set_steering_mode(QueueMode::All).await;
            assert_eq!(harness.get_steering_mode().await, QueueMode::All);
            harness.set_follow_up_mode(QueueMode::All).await;
            assert_eq!(harness.get_follow_up_mode().await, QueueMode::All);
        });
    }

    #[test]
    fn resumed_queue_items_are_consumed_into_the_durable_transcript() {
        rt().block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("resumed response")],
                FauxAssistantOptions::default(),
            ))]);
            let stream_fn: crate::agent::StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));

            let mut session = create_session("resumed-queue");
            session
                .append_record(NewRecord::OperationStarted {
                    id: "resumed-op".to_string(),
                    lane: "main".to_string(),
                    source_leaf_id: None,
                    intent: OperationIntent::Run {
                        original_prompt: vec![],
                        initial_messages: vec![],
                        system_prompt_override: None,
                        resume_data: None,
                    },
                })
                .await
                .unwrap();
            session
                .append_record(NewRecord::QueueEnqueued {
                    id: "resumed-queue-record".to_string(),
                    lane: "main".to_string(),
                    queue: "steer".to_string(),
                    run_id: "resumed-op".to_string(),
                    target: serde_json::to_value(EntryNoStats::Message {
                        id: "resumed-steer".to_string(),
                        message: user_message("resume this steer"),
                        terminate: None,
                    })
                    .unwrap(),
                })
                .await
                .unwrap();
            let mut options = AgentHarnessOptions::new(session, test_model());
            options.stream_fn = Some(stream_fn);
            let (harness, suspended) = AgentHarness::create(options).await.unwrap();
            assert_eq!(suspended.len(), 1);
            assert_eq!(
                harness.watch().await.unwrap().snapshot.queues.steer.len(),
                1
            );

            let resumed = harness.resume().await.unwrap();
            assert!(matches!(
                resumed,
                ResumeOutcome::Run {
                    run_id,
                    outcome: RunOutcome::Completed { .. }
                } if run_id == "resumed-op"
            ));
            let entries = harness.transcript().await.unwrap();
            assert!(entries.iter().any(|entry| {
                entry.id() == "resumed-steer"
                    && matches!(
                        entry.as_message(),
                        Some(AgentMessage::Core(Message::User(user)))
                            if crate::agent::user_content_text(user) == "resume this steer"
                    )
            }));
            assert_eq!(
                harness.cancel_queued("resumed-steer").await.unwrap(),
                CancelQueuedOutcome::AlreadyConsumed
            );
            assert!(harness
                .watch()
                .await
                .unwrap()
                .snapshot
                .queues
                .steer
                .is_empty());
        });
    }

    #[test]
    fn reports_explicit_runtime_and_idle_boundaries() {
        rt().block_on(async {
            let harness = create_harness().await;
            let callback_called = Arc::new(AtomicBool::new(false));
            let message = user_message("hello");
            assert!(matches!(
                harness.prompt_text("hello", &[]).await,
                Err(HarnessError::Fault { message })
                    if message == "AgentHarness.prompt requires a configured stream function"
            ));
            assert!(matches!(
                harness.skill("skill", None).await,
                Err(HarnessError::Fault { message })
                    if message == "AgentHarness.skill requires a configured stream function"
            ));
            assert!(matches!(
                harness.prompt_from_template("template", None).await,
                Err(HarnessError::Fault { message })
                    if message == "AgentHarness.promptFromTemplate requires a configured stream function"
            ));
            assert!(matches!(
                harness.compact(None).await,
                Err(HarnessError::Tagged(tag)) if tag.tag == "NothingToCompact"
            ));
            let navigation = harness.navigate_tree(None, None).await.unwrap();
            assert!(matches!(
                navigation.outcome,
                NavigationOutcome::Completed {
                    new_leaf_id: None,
                    summary_entry: None,
                }
            ));
            assert!(matches!(
                harness.resume().await,
                Err(HarnessError::Tagged(tag)) if tag.tag == "NothingToResume"
            ));
            assert!(matches!(
                harness.abort().await,
                Err(HarnessError::Tagged(tag)) if tag.tag == "NoActiveOperation"
            ));
            for result in [
                harness.steer_message(&message).await,
                harness.follow_up_message(&message).await,
                harness.next_run_message(&message).await,
            ] {
                assert!(
                    matches!(result, Err(HarnessError::Tagged(tag)) if tag.tag == "NoActiveRun")
                );
            }
            assert!(matches!(
                harness.cancel_queued("queued").await,
                Err(HarnessError::Tagged(tag)) if tag.tag == "UnknownQueueItem"
            ));
            assert!(harness.record_usage(&usage(), None).await.is_ok());
            harness.wait_for_idle().await.unwrap();
            let cb_flag = callback_called.clone();
            harness
                .run_when_idle(Arc::new(move || {
                    let flag = cb_flag.clone();
                    Box::pin(async move { flag.store(true, Ordering::Relaxed) })
                }))
                .await
                .unwrap();
            assert!(callback_called.load(Ordering::Relaxed));
            assert!(harness.peek_action().await.unwrap().is_none());
            assert!(harness.execute_action().await.unwrap().is_none());
            harness.run_to_completion().await.unwrap();
            let lane_snapshot = harness.watch().await.unwrap().snapshot;
            assert_eq!(lane_snapshot.lane, "main");
            assert!(!harness
                .watch_session()
                .await
                .unwrap()
                .snapshot
                .lanes
                .is_empty());

            let hook_unsubscribe = harness.hooks.on("before_run", Arc::new(|_| {})).unwrap();
            assert_eq!(harness.hooks.listener_count("before_run"), 1);
            hook_unsubscribe();
            assert_eq!(harness.hooks.listener_count("before_run"), 0);
            let event_unsubscribe = harness.events.on("event", Arc::new(|_| {})).unwrap();
            assert_eq!(harness.events.listener_count("event"), 1);
            event_unsubscribe();
            assert_eq!(harness.events.listener_count("event"), 0);
            assert_eq!(
                HarnessError::Closed.to_string(),
                "AgentHarness was closed while the operation was active"
            );
        });
    }

    #[test]
    fn reports_closed_for_unfinished_operations_after_close() {
        rt().block_on(async {
            let mut harness = create_harness().await;
            harness.close().await;
            assert!(matches!(
                harness.prompt_text("hello", &[]).await,
                Err(HarnessError::Closed)
            ));
            assert!(matches!(
                harness.wait_for_idle().await,
                Err(HarnessError::Closed)
            ));
            assert!(matches!(
                harness.hooks.on("before_run", Arc::new(|_| {})),
                Err(HarnessError::Closed)
            ));
            assert!(matches!(
                harness.events.on("event", Arc::new(|_| {})),
                Err(HarnessError::Closed)
            ));
        });
    }
}
