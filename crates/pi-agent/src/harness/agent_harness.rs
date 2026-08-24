//! Agent harness composition surface — port of
//! `packages/agent/src/harness/agent-harness.ts`.
//!
//! The upstream module is the scaffold on which the run loop, session,
//! compaction, resources, events, and telemetry attach. This port mirrors the
//! full public surface: tagged operation errors, outcome/result unions,
//! snapshot/action types, an `UnavailableRegistry` for hooks/events, and the
//! `AgentHarness` state holder with defensive-copy configuration getters and
//! setters. As in upstream v0.84.2, every unfinished public operation rejects
//! with `HarnessNotImplemented` (or `HarnessClosed` after `close`).
//!
//! Documented divergences:
//! - Upstream `TaggedError` subclasses (LaneBusy, MissingIdentities, ...)
//!   are flattened onto `HarnessError::Tagged(TaggedError)` with the same
//!   `_tag` strings and payload keys. `HarnessClosed`, `HarnessNotImplemented`
//!   and `HarnessFault` keep their own variants.
//! - `AgentHarnessOptions.models` (the pi-ai `Models` facade) is omitted; the
//!   harness currently reaches models through the `SimpleModels` seam
//!   (`harness/models.rs`) or explicit per-call stream functions.
//! - `toolContext` / `systemPrompt` / `toProviderMessages` accept the same
//!   logical inputs as upstream but are stored as plain values/functions at
//!   the concrete types available in the Rust port (`serde_json::Value`,
//!   `String`, message-conversion closure).
//! - `session` (upstream `SessionTree`) is the `Session<F>` facade; only one
//!   session reference is stored (upstream aliases `durableSession`/`session`).
//! - `create` rejects when the session already contains records
//!   (`HarnessNotImplemented("create.restore")`) exactly like upstream.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use pi_ai::model::Model;
use pi_ai::types::{AssistantMessage, DeferredHandle, SimpleStreamOptions, Usage};
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
use crate::harness::models::BoxFuture;
use crate::harness::result::TaggedError;
use crate::rich_agent::Agent;
use crate::session::session::Session;
use crate::session::state::{EntryOrder, EntryQuery, RecordQuery};
use crate::session::types::{Entry, EntryNoStats};
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
/// MissingIdentities, ...), `NotImplemented`/`Closed` mirror
/// `HarnessNotImplemented`/`HarnessClosed`, and `Fault` mirrors
/// `HarnessFault` (an untagged wrapper around an underlying cause).
#[derive(Debug, Clone)]
pub enum HarnessError {
    Tagged(TaggedError),
    NotImplemented { operation: String },
    Closed,
    Fault { message: String },
}

impl HarnessError {
    pub fn tagged(tag: impl Into<String>, message: impl Into<String>) -> Self {
        HarnessError::Tagged(TaggedError::new(tag, message))
    }
    pub fn not_implemented(operation: impl Into<String>) -> Self {
        HarnessError::NotImplemented {
            operation: operation.into(),
        }
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
            HarnessError::NotImplemented { operation } => {
                write!(f, "AgentHarness.{operation} is not implemented yet")
            }
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

#[derive(Debug, Clone)]
pub struct QueuedItem {
    pub entry_id: String,
    pub message: AgentMessage,
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
    pub replay: Option<ReplayPolicy>,
}

impl std::fmt::Debug for HarnessTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessTool")
            .field("name", &self.tool.name)
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
            replay: None,
        }
    }

    fn to_agent_tool(&self) -> AgentTool {
        let mut tool = AgentTool::new(self.tool.clone(), self.name(), self.execute.clone());
        if let Some(prepare_arguments) = &self.prepare_arguments {
            tool = tool.with_prepare_arguments(prepare_arguments.clone());
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
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
/// are accepted for surface parity; the scaffold stores the pure-value
/// subset and keeps the callable seams ready for the run-loop port.
pub struct AgentHarnessOptions<F: FileSystem> {
    pub session: Session<F>,
    pub model: Model,
    pub stream_fn: Option<crate::agent::StreamFn>,
    pub system_prompt: Option<String>,
    pub block_images: bool,
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
            system_prompt: None,
            block_images: false,
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
// Hooks / events registry (currently unavailable)
// ---------------------------------------------------------------------------

/// Registry that rejects every registration with `HarnessNotImplemented`
/// (or `HarnessClosed` after close). Mirrors the upstream
/// `UnavailableRegistry` used for `harness.hooks` and `harness.events`.
pub struct UnavailableRegistry {
    operation: &'static str,
    closed: Arc<RwLock<bool>>,
}

impl UnavailableRegistry {
    fn new(operation: &'static str, closed: Arc<RwLock<bool>>) -> Self {
        Self { operation, closed }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.read().map(|b| *b).unwrap_or(false)
    }

    pub fn on(&self, _name: &str, _handler: EventHandler) -> Result<UnsubscribeFn, HarnessError> {
        Err(if self.is_closed() {
            HarnessError::closed()
        } else {
            HarnessError::not_implemented(self.operation)
        })
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
/// `AgentHarness`). Sessions with existing records reject `create`.
pub struct AgentHarness<F: FileSystem> {
    name: String,
    session: Session<F>,
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
    agent: Option<Arc<Agent>>,
    telemetry_context: HarnessTelemetryContext,
    event_bus: HarnessEventBus,
    pub hooks: UnavailableRegistry,
    pub events: UnavailableRegistry,
    closed: Arc<RwLock<bool>>,
}

impl<F: FileSystem> std::fmt::Debug for AgentHarness<F> {
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

impl<F: FileSystem> AgentHarness<F> {
    /// Open a harness for a record-free session. Sessions that already
    /// contain records reject with `HarnessNotImplemented("create.restore")`
    /// until restore is ported (upstream behavior).
    pub async fn create(
        options: AgentHarnessOptions<F>,
    ) -> Result<(AgentHarness<F>, Vec<SuspendedOperation>), HarnessError> {
        let records = options
            .session
            .find_records(&RecordQuery {
                limit: Some(1),
                ..Default::default()
            })
            .await
            .map_err(HarnessError::from)?;
        if !records.is_empty() {
            return Err(HarnessError::not_implemented("create.restore"));
        }
        Ok((AgentHarness::new(options), Vec::new()))
    }

    fn new(options: AgentHarnessOptions<F>) -> Self {
        let closed = Arc::new(RwLock::new(false));
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
        let agent = options.stream_fn.clone().map(|stream_fn| {
            let mut agent = Agent::new(stream_fn);
            {
                let mut state = agent.state();
                state.model = options.model.clone();
                state.system_prompt = options.system_prompt.clone().unwrap_or_default();
                state.set_tools(
                    options
                        .tools
                        .as_ref()
                        .map(|tools| tools.iter().map(HarnessTool::to_agent_tool).collect())
                        .unwrap_or_default(),
                );
            }
            agent.set_block_images(options.block_images);
            if let Some(ToolExecution::Sequential) = options.tool_execution {
                agent.set_tool_execution(crate::rich_agent::ToolExecutionMode::Sequential);
            }
            Arc::new(agent)
        });
        Self {
            name: "main".to_string(),
            session: options.session,
            model: options.model,
            thinking_level: options.thinking_level.unwrap_or(ModelThinkingLevel::Off),
            active_tool_names,
            tools: options.tools.clone().unwrap_or_default(),
            resources: options.resources.clone().unwrap_or_default(),
            stream_options: options.stream_options.clone().unwrap_or_default(),
            retry_policy,
            compaction_settings,
            steering_mode: options.steering_mode.unwrap_or_default(),
            follow_up_mode: options.follow_up_mode.unwrap_or_default(),
            agent,
            telemetry_context,
            event_bus: HarnessEventBus::new(),
            hooks: UnavailableRegistry::new("hooks.on", closed.clone()),
            events: UnavailableRegistry::new("events.on", closed.clone()),
            closed,
        }
    }

    /// The underlying durable session tree (upstream aliases
    /// `durableSession` and `session` to the same object).
    pub fn session(&self) -> &Session<F> {
        &self.session
    }

    pub fn is_closed(&self) -> bool {
        self.closed.read().map(|b| *b).unwrap_or(false)
    }

    /// Subscribe to the integrated harness run lifecycle without changing
    /// the upstream scaffolded `events` registry surface.
    pub fn subscribe_event(
        &mut self,
        event_type: &'static str,
        listener: HarnessEventListener,
    ) -> usize {
        self.event_bus.on(event_type, listener)
    }

    pub fn unsubscribe_event(&mut self, subscription_id: usize) {
        self.event_bus.unsubscribe(subscription_id);
    }

    fn unavailable<T>(&self, operation: &str) -> Result<T, HarnessError> {
        Err(if self.is_closed() {
            HarnessError::closed()
        } else {
            HarnessError::not_implemented(operation)
        })
    }

    /// Run prompts through the configured stateful Agent and append the
    /// resulting messages to the harness-owned main-lane session. A harness
    /// created without a stream function remains a scaffold and reports the
    /// same explicit `HarnessNotImplemented` error as upstream.
    pub async fn run_prompt(
        &mut self,
        prompts: Vec<AgentMessage>,
    ) -> Result<Vec<AgentMessage>, HarnessError> {
        if self.is_closed() {
            return Err(HarnessError::closed());
        }
        let Some(agent) = self.agent.clone() else {
            return self.unavailable("prompt");
        };
        let run_id = crate::session::new_id();
        let session_id = self.session.get_metadata().await.id;
        self.event_bus.emit(&HarnessEvent::RunStart(RunStartEvent {
            lane: self.name.clone(),
            run_id: run_id.clone(),
        }));

        let telemetry = self.telemetry_context.clone();
        let span_run_id = run_id.clone();
        let span_session_id = session_id;
        let span_lane = self.name.clone();
        let session = &mut self.session;
        let run_result: Result<Vec<AgentMessage>, HarnessError> = telemetry
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
                    let messages = agent.prompt_messages(prompts).await;
                    for message in &messages {
                        if let Err(error) = session.append_message(message.clone()).await {
                            span.set_status(SpanStatus::Error {
                                error: Some(SpanError {
                                    name: "SessionError".to_string(),
                                    message: error.to_string(),
                                }),
                            });
                            return Err(HarnessError::from(error));
                        }
                    }
                    span.set_attributes(BTreeMap::from([(
                        "pi.operation.outcome".to_string(),
                        serde_json::json!("completed"),
                    )]));
                    span.add_event("run_end", None);
                    Ok(messages)
                },
            )
            .await;

        let leaf_id = self
            .session
            .get_leaf_id()
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        self.event_bus.emit(&HarnessEvent::RunEnd(RunEndEvent {
            lane: self.name.clone(),
            run_id,
            outcome: if run_result.is_ok() {
                EventOutcome::Completed
            } else {
                EventOutcome::Failed
            },
            leaf_id,
        }));
        run_result
    }

    /// Snapshot the harness-owned durable transcript in chronological order.
    pub async fn transcript(&self) -> Result<Vec<Entry>, HarnessError> {
        self.session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .map_err(HarnessError::from)
    }

    /// Append a provisioned entry to the harness-owned main lane.
    pub async fn append_entry(&mut self, entry: EntryNoStats) -> Result<Entry, HarnessError> {
        self.session
            .append_entry(entry, "main")
            .await
            .map_err(HarnessError::from)
    }

    /// Replace the in-memory Agent transcript after a compaction boundary.
    pub async fn set_agent_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), HarnessError> {
        let Some(agent) = &self.agent else {
            return self.unavailable("prompt");
        };
        agent.state().set_messages(messages);
        Ok(())
    }

    pub async fn agent_messages(&self) -> Result<Vec<AgentMessage>, HarnessError> {
        let Some(agent) = &self.agent else {
            return self.unavailable("prompt");
        };
        Ok(agent.state().messages().to_vec())
    }

    /// Lane accessor on the main harness (upstream `lane(name)`); unimplemented
    /// until secondary lanes land.
    pub async fn lane(&self, _name: &str) -> Result<Box<dyn AgentLane>, HarnessError> {
        self.unavailable("lane")
    }

    /// Create a new lane (upstream `createLane(name, at)`); unimplemented.
    pub async fn create_lane(
        &self,
        _name: &str,
        _at: Option<&str>,
    ) -> Result<Box<dyn AgentLane>, HarnessError> {
        self.unavailable("createLane")
    }

    /// List lanes (upstream `lanes()`); unimplemented.
    pub async fn lanes(&self) -> Result<Vec<LaneInfo>, HarnessError> {
        self.unavailable("lanes")
    }
}

#[async_trait]
impl<F: FileSystem> AgentLane for AgentHarness<F> {
    fn lane_name(&self) -> &str {
        &self.name
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.session.get_leaf_id().await
    }

    async fn prompt_text(
        &self,
        _text: &str,
        _images: &[ImageContent],
    ) -> Result<RunResultValue, HarnessError> {
        self.unavailable("prompt")
    }

    async fn prompt_messages(
        &self,
        _messages: &[AgentMessage],
    ) -> Result<RunResultValue, HarnessError> {
        self.unavailable("prompt")
    }

    async fn skill(
        &self,
        _name: &str,
        _additional_instructions: Option<&str>,
    ) -> Result<RunResultValue, HarnessError> {
        self.unavailable("skill")
    }

    async fn prompt_from_template(
        &self,
        _name: &str,
        _args: Option<&[String]>,
    ) -> Result<RunResultValue, HarnessError> {
        self.unavailable("promptFromTemplate")
    }

    async fn compact(
        &self,
        _custom_instructions: Option<&str>,
    ) -> Result<CompactionResultValue, HarnessError> {
        self.unavailable("compact")
    }

    async fn navigate_tree(
        &self,
        _target_id: Option<&str>,
        _options: Option<&NavigateOptions>,
    ) -> Result<NavigationResultValue, HarnessError> {
        self.unavailable("navigateTree")
    }

    async fn resume(&self) -> Result<ResumeOutcome, HarnessError> {
        self.unavailable("resume")
    }

    async fn abort(&self) -> Result<AbortResultValue, HarnessError> {
        self.unavailable("abort")
    }

    async fn steer_text(
        &self,
        _text: &str,
        _images: &[ImageContent],
    ) -> Result<String, HarnessError> {
        self.unavailable("steer")
    }

    async fn steer_message(&self, _message: &AgentMessage) -> Result<String, HarnessError> {
        self.unavailable("steer")
    }

    async fn follow_up_text(
        &self,
        _text: &str,
        _images: &[ImageContent],
    ) -> Result<String, HarnessError> {
        self.unavailable("followUp")
    }

    async fn follow_up_message(&self, _message: &AgentMessage) -> Result<String, HarnessError> {
        self.unavailable("followUp")
    }

    async fn next_run_text(
        &self,
        _text: &str,
        _images: &[ImageContent],
    ) -> Result<String, HarnessError> {
        self.unavailable("nextRun")
    }

    async fn next_run_message(&self, _message: &AgentMessage) -> Result<String, HarnessError> {
        self.unavailable("nextRun")
    }

    async fn cancel_queued(&self, _entry_id: &str) -> Result<CancelQueuedOutcome, HarnessError> {
        self.unavailable("cancelQueued")
    }

    async fn record_usage(
        &self,
        _usage: &Usage,
        _options: Option<&RecordUsageOptions>,
    ) -> Result<(), HarnessError> {
        self.unavailable("recordUsage")
    }

    async fn wait_for_idle(&self) -> Result<(), HarnessError> {
        self.unavailable("waitForIdle")
    }

    async fn run_when_idle(&self, _callback: RunWhenIdleCallback) -> Result<(), HarnessError> {
        self.unavailable("runWhenIdle")
    }

    async fn peek_action(&self) -> Result<Option<ActionInfo>, HarnessError> {
        self.unavailable("peekAction")
    }

    async fn execute_action(&self) -> Result<Option<ActionInfo>, HarnessError> {
        self.unavailable("executeAction")
    }

    async fn run_to_completion(&self) -> Result<(), HarnessError> {
        self.unavailable("runToCompletion")
    }

    async fn get_model(&self) -> Model {
        self.model.clone()
    }

    async fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    async fn get_thinking_level(&self) -> ModelThinkingLevel {
        self.thinking_level
    }

    async fn set_thinking_level(&mut self, level: ModelThinkingLevel) {
        self.thinking_level = level;
    }

    async fn get_active_tools(&self) -> Vec<String> {
        self.active_tool_names.clone()
    }

    async fn set_active_tools(&mut self, names: Vec<String>) {
        self.active_tool_names = names;
    }

    async fn get_tools(&self) -> Vec<HarnessTool> {
        self.tools.clone()
    }

    async fn set_tools(&mut self, tools: Vec<HarnessTool>, active_names: Option<Vec<String>>) {
        self.tools = tools.clone();
        self.active_tool_names =
            active_names.unwrap_or_else(|| tools.iter().map(|t| t.name().to_string()).collect());
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
        self.stream_options = options;
    }

    async fn get_retry_policy(&self) -> RetryPolicy {
        self.retry_policy.clone()
    }

    async fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry_policy = policy;
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
    }

    async fn get_follow_up_mode(&self) -> QueueMode {
        self.follow_up_mode
    }

    async fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
    }

    async fn watch(&self) -> Result<WatchHandle<LaneSnapshot>, HarnessError> {
        self.unavailable("watch")
    }

    async fn watch_session(&self) -> Result<WatchHandle<SessionSnapshot>, HarnessError> {
        self.unavailable("watchSession")
    }

    async fn close(&mut self) {
        if let Ok(mut closed) = self.closed.write() {
            *closed = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Scaffold tests (ported from agent-harness-scaffold.test.ts)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MemoryFs;
    use crate::session::memory::{in_memory_metadata, InMemorySessionStorage};
    use crate::session::types::{NewRecord, OperationIntent};
    use pi_ai::providers::{
        faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
        RegisterFauxProviderOptions,
    };
    use pi_ai::types::{ContentBlock, Cost, Message, UserContent};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

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

    fn not_impl(err: &HarnessError) -> &str {
        match err {
            HarnessError::NotImplemented { operation } => operation,
            other => panic!("expected NotImplemented, got {other:?}"),
        }
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
                        .unwrap()
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
                        .unwrap()
                        .push(format!("{}:{outcome}", event.event_type()));
                }),
            );

            let messages = harness
                .run_prompt(vec![user_message("hello")])
                .await
                .unwrap();
            assert_eq!(messages.len(), 2);
            assert_eq!(harness.agent_messages().await.unwrap().len(), 2);

            let transcript = harness.transcript().await.unwrap();
            assert_eq!(transcript.len(), 2);
            assert_eq!(transcript[0].as_message().unwrap(), &messages[0]);
            assert_eq!(transcript[1].as_message().unwrap(), &messages[1]);
            assert_eq!(
                *lifecycle.lock().unwrap(),
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
                Box::new(move |_| start_lifecycle.lock().unwrap().push("start".into())),
            );
            let end_lifecycle = lifecycle.clone();
            bus.on(
                "run_end",
                Box::new(move |event| {
                    end_lifecycle.lock().unwrap().push(format!(
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
                *lifecycle.lock().unwrap(),
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
    fn opens_only_record_free_sessions_before_restore_is_implemented() {
        rt().block_on(async {
            let session = create_session("session");
            let (mut harness, suspended) =
                AgentHarness::create(AgentHarnessOptions::new(session, test_model()))
                    .await
                    .unwrap();
            assert!(suspended.is_empty());
            assert_eq!(harness.lane_name(), "main");
            assert_eq!(harness.get_leaf_id().await.unwrap(), None);
            assert_eq!(harness.session().get_leaf_id().await.unwrap(), None);
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
            let err = AgentHarness::create(AgentHarnessOptions::new(recorded, test_model()))
                .await
                .unwrap_err();
            assert_eq!(not_impl(&err), "create.restore");
        });
    }

    #[test]
    fn keeps_scaffold_safe_configuration_as_defensive_copies() {
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
    fn rejects_every_unfinished_public_operation_explicitly() {
        rt().block_on(async {
            let harness = create_harness().await;
            let callback_called = Arc::new(AtomicBool::new(false));
            let message = user_message("hello");
            let usage = usage();
            let mut error_by_op: std::collections::BTreeMap<String, HarnessError> =
                Default::default();

            fn capture(
                key: &str,
                map: &mut std::collections::BTreeMap<String, HarnessError>,
                err: HarnessError,
            ) {
                map.insert(key.to_string(), err);
            }

            capture(
                "prompt",
                &mut error_by_op,
                harness.prompt_text("hello", &[]).await.unwrap_err(),
            );
            capture(
                "skill",
                &mut error_by_op,
                harness.skill("skill", None).await.unwrap_err(),
            );
            capture(
                "promptFromTemplate",
                &mut error_by_op,
                harness
                    .prompt_from_template("template", None)
                    .await
                    .unwrap_err(),
            );
            capture(
                "compact",
                &mut error_by_op,
                harness.compact(None).await.unwrap_err(),
            );
            capture(
                "navigateTree",
                &mut error_by_op,
                harness.navigate_tree(None, None).await.unwrap_err(),
            );
            capture(
                "resume",
                &mut error_by_op,
                harness.resume().await.unwrap_err(),
            );
            capture(
                "abort",
                &mut error_by_op,
                harness.abort().await.unwrap_err(),
            );
            capture(
                "steer",
                &mut error_by_op,
                harness.steer_message(&message).await.unwrap_err(),
            );
            capture(
                "followUp",
                &mut error_by_op,
                harness.follow_up_message(&message).await.unwrap_err(),
            );
            capture(
                "nextRun",
                &mut error_by_op,
                harness.next_run_message(&message).await.unwrap_err(),
            );
            capture(
                "cancelQueued",
                &mut error_by_op,
                harness.cancel_queued("queued").await.unwrap_err(),
            );
            capture(
                "recordUsage",
                &mut error_by_op,
                harness.record_usage(&usage, None).await.unwrap_err(),
            );
            capture(
                "waitForIdle",
                &mut error_by_op,
                harness.wait_for_idle().await.unwrap_err(),
            );
            {
                let cb_flag = callback_called.clone();
                let err = harness
                    .run_when_idle(Arc::new(move || {
                        let flag = cb_flag.clone();
                        Box::pin(async move { flag.store(true, Ordering::Relaxed) })
                    }))
                    .await
                    .unwrap_err();
                capture("runWhenIdle", &mut error_by_op, err);
            }
            capture(
                "peekAction",
                &mut error_by_op,
                harness.peek_action().await.unwrap_err(),
            );
            capture(
                "executeAction",
                &mut error_by_op,
                harness.execute_action().await.unwrap_err(),
            );
            capture(
                "runToCompletion",
                &mut error_by_op,
                harness.run_to_completion().await.unwrap_err(),
            );
            {
                let err = match harness.watch().await {
                    Err(e) => e,
                    Ok(_) => panic!("watch unexpectedly implemented"),
                };
                capture("watch", &mut error_by_op, err);
            }
            {
                let err = match harness.lane("main").await {
                    Err(e) => e,
                    Ok(_) => panic!("lane unexpectedly implemented"),
                };
                capture("lane", &mut error_by_op, err);
            }
            {
                let err = match harness.create_lane("thread", None).await {
                    Err(e) => e,
                    Ok(_) => panic!("createLane unexpectedly implemented"),
                };
                capture("createLane", &mut error_by_op, err);
            }
            capture(
                "lanes",
                &mut error_by_op,
                harness.lanes().await.unwrap_err(),
            );
            {
                let err = match harness.watch_session().await {
                    Err(e) => e,
                    Ok(_) => panic!("watchSession unexpectedly implemented"),
                };
                capture("watchSession", &mut error_by_op, err);
            }

            let checks: Vec<&str> = vec![
                "prompt",
                "skill",
                "promptFromTemplate",
                "compact",
                "navigateTree",
                "resume",
                "abort",
                "steer",
                "followUp",
                "nextRun",
                "cancelQueued",
                "recordUsage",
                "waitForIdle",
                "runWhenIdle",
                "peekAction",
                "executeAction",
                "runToCompletion",
                "watch",
                "lane",
                "createLane",
                "lanes",
                "watchSession",
            ];
            for key in checks {
                let err = error_by_op
                    .get(key)
                    .unwrap_or_else(|| panic!("missing {key}"));
                assert_eq!(not_impl(err), key, "{key}");
            }
            assert!(!callback_called.load(Ordering::Relaxed));
            assert!(harness.hooks.on("before_run", Arc::new(|_| {})).is_err());
            assert!(harness.events.on("event", Arc::new(|_| {})).is_err());
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
