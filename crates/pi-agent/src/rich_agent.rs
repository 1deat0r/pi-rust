//! Rich agent surface — additive port of `packages/agent/src/agent.ts` +
//! `agent-loop.ts` on top of the existing tool contract.
//!
//! Provides the upstream-shaped loop (`PendingMessageQueue`, `QueueMode`,
//! `ToolExecutionMode`), a full event stream (`message_update` /
//! `tool_execution_*`), steering/follow-up drains, before/afterToolCall hooks,
//! sequential+parallel tool batches, and the stateful `Agent` class.
//!
//! Contract notes:
//! - Tool update callbacks are scoped to an execution and are converted into
//!   `tool_execution_update` events before the matching end event.
//! - Tool `terminate` hints are retained in the in-memory result and control
//!   batch termination; they are intentionally not added to the model-facing
//!   `ToolResultMessage` shape.
//! - `validateToolArguments` runs after each tool's `prepareArguments` shim.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{FutureExt, StreamExt};
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, Context, Message, StopReason, ThinkingLevel,
    ToolResultMessage,
};

use crate::agent::{is_aborted, AgentContext, StreamFn, StreamFnWithOptions};
use crate::tools::{AgentTool, AgentToolResult};
use crate::types::AgentMessage;

/// How queued messages are injected at a drain point (upstream `QueueMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// Queue of pending steering/follow-up messages (upstream
/// `PendingMessageQueue`).
#[derive(Debug, Clone)]
pub struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    pub mode: QueueMode,
}

impl PendingMessageQueue {
    pub fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }
    pub fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }
    pub fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }
    pub fn len(&self) -> usize {
        self.messages.len()
    }
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
    pub fn snapshot(&self) -> Vec<AgentMessage> {
        self.messages.clone()
    }
    pub fn drain(&mut self) -> Vec<AgentMessage> {
        if self.mode == QueueMode::All {
            return std::mem::take(&mut self.messages);
        }
        if self.messages.is_empty() {
            return Vec::new();
        }
        vec![self.messages.remove(0)]
    }
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Tool execution strategy for assistant messages that contain multiple tool
/// calls (upstream `ToolExecutionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// Events emitted by the `Agent` for UI updates (upstream `AgentEvent`).
#[allow(clippy::large_enum_variant)] // preserve the public upstream event shape
#[derive(Debug, Clone, PartialEq)]
pub enum RichAgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: serde_json::Value,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        /// Raw upstream result, including the internal `terminate` hint.
        /// The model-facing `ToolResultMessage` is emitted separately and
        /// intentionally omits that hint.
        result: AgentToolResult,
        is_error: bool,
    },
}

/// `beforeToolCall` hook result (upstream `BeforeToolCallResult`).
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

/// `afterToolCall` overrides applied by the loop (upstream
/// `AfterToolCallResult` field merge).
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<pi_ai::types::ContentBlock>>,
    pub details: Option<serde_json::Value>,
    pub usage: Option<pi_ai::types::Usage>,
    pub added_tool_names: Option<Vec<String>>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

/// Context passed to `beforeToolCall`.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}

/// Context passed to `afterToolCall`.
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: ToolResultMessage,
    pub is_error: bool,
}

/// Async hook that returns a future.
pub type AsyncHook<Args, Out> =
    Arc<dyn Fn(Args) -> Pin<Box<dyn Future<Output = Out> + Send>> + Send + Sync>;

pub type ConvertToLlmFn = Arc<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;

pub type TransformContextHook =
    AsyncHook<(Vec<AgentMessage>, Option<Arc<AtomicBool>>), Vec<AgentMessage>>;
pub type ApiKeyResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
type AgentListener = Arc<
    dyn Fn(RichAgentEvent, Option<Arc<AtomicBool>>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

pub type BeforeToolCallHook = Arc<
    dyn for<'a> Fn(
            &'a mut BeforeToolCallContext,
            Option<Arc<AtomicBool>>,
        )
            -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send + 'a>>
        + Send
        + Sync,
>;

type BeforeToolCallFuture<'a> =
    Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send + 'a>>;
type BeforeToolCallInvocation<'a> = Result<BeforeToolCallFuture<'a>, Box<dyn std::any::Any + Send>>;

fn invoke_before_tool_call<'a>(
    hook: &'a BeforeToolCallHook,
    context: &'a mut BeforeToolCallContext,
    signal: Option<Arc<AtomicBool>>,
) -> BeforeToolCallInvocation<'a> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(context, signal)))
}

pub type AfterToolCallHook = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;

pub type ShouldStopAfterTurnHook = Arc<
    dyn Fn(AssistantMessage, Vec<ToolResultMessage>) -> Pin<Box<dyn Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// Context supplied to the next-turn preparation hooks. It is a snapshot of
/// the state after `turn_end`, so a hook can inspect the completed turn without
/// borrowing the loop across an await point.
#[derive(Clone)]
pub struct PrepareNextTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: Vec<AgentMessage>,
}

/// Replacement state used before the next provider request. `thinking_level`
/// is nested so `Some(None)` means explicitly turn reasoning off while
/// `None` leaves the current setting unchanged.
#[derive(Clone, Default)]
pub struct RichAgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<pi_ai::model::Model>,
    pub thinking_level: Option<Option<ThinkingLevel>>,
    /// Optional complete stream-option replacement for the next turn.
    pub stream_options: Option<pi_ai::types::SimpleStreamOptions>,
}

pub type PrepareNextTurnHook = Arc<
    dyn Fn(
            Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn Future<Output = Option<RichAgentLoopTurnUpdate>> + Send>>
        + Send
        + Sync,
>;

pub type PrepareNextTurnWithContextHook = Arc<
    dyn Fn(
            PrepareNextTurnContext,
            Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn Future<Output = Option<RichAgentLoopTurnUpdate>> + Send>>
        + Send
        + Sync,
>;

/// Why the provider response entered the automatic compaction path.
///
/// This deliberately mirrors the two automatic overflow cases in
/// `packages/coding-agent/src/core/agent-session.ts`: a provider-reported (or
/// usage-detected) context overflow, and a response truncated below the
/// requested output limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowRecoveryReason {
    ContextOverflow,
    RecoverableLength,
}

impl OverflowRecoveryReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextOverflow => "overflow",
            Self::RecoverableLength => "recoverable_length",
        }
    }
}

/// Input supplied to the production compaction/session seam.
///
/// `durable_messages` contains the response that caused recovery. A session
/// implementation should persist that history before/while compacting it.
/// `retry_messages` is the exact pre-response context and is provided so the
/// resulting active context cannot accidentally make the failed response part
/// of the retry request. The callback owns the real compaction provider call
/// and durable session append; the shared loop does not manufacture a summary
/// or use a test/faux provider.
#[derive(Clone)]
pub struct OverflowRecoveryRequest {
    pub reason: OverflowRecoveryReason,
    pub assistant_message: AssistantMessage,
    pub model: pi_ai::model::Model,
    pub context: AgentContext,
    pub durable_messages: Vec<AgentMessage>,
    pub retry_messages: Vec<AgentMessage>,
    pub will_retry: bool,
}

/// Post-compaction active context returned by [`OverflowRecoveryHook`].
///
/// For `will_retry == true`, the shared loop removes any exact copy of the
/// failed assistant response from this context before making the next
/// provider request. For `will_retry == false`, the response is retained as
/// required by the successful-overflow branch of the upstream session.
#[derive(Clone)]
pub struct OverflowRecoveryResult {
    pub context: AgentContext,
}

/// Production compaction/session callback used by the rich loop.
///
/// The callback should run the real compaction implementation against the
/// caller's durable session and return the rebuilt, provider-ready active
/// context. Returning an error leaves the overflow response as the terminal
/// result. A caller that does not configure this hook intentionally retains
/// the old terminal-error behavior instead of silently compacting with a
/// fabricated summary.
pub type OverflowRecoveryHook = Arc<
    dyn Fn(
            OverflowRecoveryRequest,
            Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn Future<Output = Result<OverflowRecoveryResult, String>> + Send>>
        + Send
        + Sync,
>;

/// Errors raised when a stateful agent operation cannot start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentRunError {
    #[error(
        "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion."
    )]
    AlreadyProcessingPrompt,
    #[error("Agent is already processing. Wait for completion before continuing.")]
    AlreadyProcessingContinuation,
    #[error("No messages to continue from")]
    NoMessagesToContinue,
    #[error("Cannot continue from message role: assistant")]
    CannotContinueFromAssistant,
}

fn none_hook<Out: Default>() -> AsyncHook<(), Out> {
    Arc::new(|()| async { Out::default() }.boxed())
}

/// Full agent-loop configuration (upstream `AgentLoopConfig`).
#[derive(Clone)]
pub struct RichAgentLoopConfig {
    pub model: pi_ai::model::Model,
    /// Provider stream function.
    pub stream_fn: StreamFn,
    /// Option-aware provider stream function. When present it is preferred to
    /// `stream_fn`; the legacy function remains available for compatibility.
    pub stream_fn_with_options: Option<StreamFnWithOptions>,
    /// Abort flag for the run.
    pub signal: Option<Arc<AtomicBool>>,
    /// Convert `AgentMessage[]` to LLM `Message[]` before each call. Defaults
    /// to the harness converter (custom messages rendered for the provider).
    pub convert_to_llm: Option<ConvertToLlmFn>,
    /// Replace image blocks at the provider boundary while keeping them in
    /// the durable transcript/UI result.
    pub block_images: bool,
    /// Normalize images in finalized tool results after `after_tool_call`.
    /// `None` keeps the provider-neutral agent behavior; coding-agent enables
    /// this with its persisted image processing setting.
    pub tool_result_image_options: Option<crate::tools::image::ProcessImageOptions>,
    /// Optional transform applied at the AgentMessage level before conversion.
    pub transform_context: Option<TransformContextHook>,
    /// Dynamic API key resolver for each LLM call.
    pub get_api_key: Option<ApiKeyResolver>,
    /// Reasoning level forwarded to the stream function.
    pub reasoning: Option<ThinkingLevel>,
    /// Session identifier forwarded to cache-aware providers.
    pub session_id: Option<String>,
    /// Complete provider-neutral options merged into each request. The
    /// explicit `reasoning`, `session_id`, `on_payload`, and `on_response`
    /// fields below take precedence when set.
    pub stream_options: pi_ai::types::SimpleStreamOptions,
    /// Inspect or replace the provider payload before it is sent.
    pub on_payload: Option<pi_ai::types::OnPayloadFn>,
    /// Observe the provider response before its body is consumed.
    pub on_response: Option<pi_ai::model::OnResponseFn>,
    /// Cap for provider-requested retry delays.
    pub max_retry_delay_ms: Option<u64>,
    /// Tool execution strategy for multi-tool batches.
    pub tool_execution: ToolExecutionMode,
    /// Hook invoked after tool arguments are validated.
    pub before_tool_call: Option<BeforeToolCallHook>,
    /// Hook invoked after a tool executes, before `tool_execution_end`.
    pub after_tool_call: Option<AfterToolCallHook>,
    /// Returns true when the loop should exit after this turn.
    pub should_stop_after_turn: Option<ShouldStopAfterTurnHook>,
    /// Prepare the next turn after `turn_end`.
    pub prepare_next_turn: Option<PrepareNextTurnHook>,
    /// Context-aware form of `prepare_next_turn`; takes precedence when set.
    pub prepare_next_turn_with_context: Option<PrepareNextTurnWithContextHook>,
    /// Real session-backed compaction seam for provider overflow recovery.
    /// The loop permits at most one compact-and-retry for a response.
    pub overflow_recovery: Option<OverflowRecoveryHook>,
    /// Returns steering messages to inject mid-run.
    pub get_steering_messages: AsyncHook<(), Vec<AgentMessage>>,
    /// Returns follow-up messages to process after the agent would stop.
    pub get_follow_up_messages: AsyncHook<(), Vec<AgentMessage>>,
    /// Explicit API key (applied when `get_api_key` returns none).
    pub api_key: Option<String>,
    /// Optional retry policy for transient assistant errors.
    pub retry_policy: Option<pi_ai::utils::retry::RetryPolicy>,
    /// Separate cancellation flag for a pending retry backoff.
    pub retry_signal: Option<Arc<AtomicBool>>,
}

impl RichAgentLoopConfig {
    /// Build a config with upstream field defaults (no-op hooks).
    pub fn new(
        model: pi_ai::model::Model,
        stream_fn: StreamFn,
        signal: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            model,
            stream_fn,
            stream_fn_with_options: None,
            signal,
            convert_to_llm: None,
            block_images: false,
            tool_result_image_options: None,
            transform_context: None,
            get_api_key: None,
            reasoning: None,
            session_id: None,
            stream_options: pi_ai::types::SimpleStreamOptions::default(),
            on_payload: None,
            on_response: None,
            max_retry_delay_ms: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            prepare_next_turn_with_context: None,
            overflow_recovery: None,
            get_steering_messages: none_hook(),
            get_follow_up_messages: none_hook(),
            api_key: None,
            retry_policy: None,
            retry_signal: None,
        }
    }
}

fn tool_calls_of(message: &AssistantMessage) -> Vec<ToolCallRef<'_>> {
    message
        .content()
        .iter()
        .filter_map(|c| match c {
            pi_ai::types::ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(ToolCallRef {
                id: id.as_str(),
                name: name.as_str(),
                arguments,
            }),
            _ => None,
        })
        .collect()
}

struct ToolCallRef<'a> {
    id: &'a str,
    name: &'a str,
    arguments: &'a serde_json::Value,
}

/// Result of a tool-batch execution (upstream `ExecutedToolCallBatch`).
pub struct ExecutedToolBatch {
    pub messages: Vec<ToolResultMessage>,
    /// True when every finalized tool result in the batch set `terminate`
    /// (upstream `shouldTerminateToolBatch`).
    pub terminate: bool,
}

fn create_error_agent_tool_result(message: &str) -> crate::tools::AgentToolResult {
    crate::tools::AgentToolResult::text(message)
}

/// Convert an `AgentToolResult` into a `ToolResultMessage` (upstream
/// `createToolResultMessage`).
fn agent_tool_result_to_message(
    tool_call_id: &str,
    tool_name: &str,
    result: &crate::tools::AgentToolResult,
    is_error: bool,
) -> ToolResultMessage {
    ToolResultMessage::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: result.content.clone(),
        details: result.details.clone(),
        usage: result.usage.clone(),
        added_tool_names: if result.added_tool_names.is_empty() {
            None
        } else {
            Some(result.added_tool_names.clone())
        },
        is_error,
        timestamp: pi_ai::types::now_ms(),
    }
}

/// Serialize the upstream-shaped partial result carried by a
/// `tool_execution_update` event. Optional fields are omitted just as they
/// are from the TypeScript `AgentToolResult` object.
/// Serialize an upstream-shaped `AgentToolResult` for tool lifecycle events.
/// Optional fields are omitted and `added_tool_names` uses the upstream
/// `addedToolNames` spelling.
pub fn agent_tool_result_to_partial_json(result: &AgentToolResult) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert(
        "content".to_string(),
        serde_json::to_value(&result.content).unwrap_or(serde_json::Value::Null),
    );
    if let Some(details) = &result.details {
        object.insert("details".to_string(), details.clone());
    }
    if let Some(usage) = &result.usage {
        object.insert(
            "usage".to_string(),
            serde_json::to_value(usage).unwrap_or(serde_json::Value::Null),
        );
    }
    if !result.added_tool_names.is_empty() {
        object.insert(
            "addedToolNames".to_string(),
            serde_json::json!(result.added_tool_names),
        );
    }
    if result.terminate {
        object.insert("terminate".to_string(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(object)
}

struct ToolUpdateEvent {
    tool_call_id: String,
    tool_name: String,
    args: serde_json::Value,
    partial_result: crate::tools::AgentToolResult,
}

fn emit_tool_update<F>(update: ToolUpdateEvent, emit: &mut F)
where
    F: FnMut(RichAgentEvent) + Send,
{
    emit(RichAgentEvent::ToolExecutionUpdate {
        tool_call_id: update.tool_call_id,
        tool_name: update.tool_name,
        args: update.args,
        partial_result: agent_tool_result_to_partial_json(&update.partial_result),
    });
}

fn drain_queued_tool_updates<F>(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<ToolUpdateEvent>,
    emit: &mut F,
) where
    F: FnMut(RichAgentEvent) + Send,
{
    while let Ok(update) = receiver.try_recv() {
        emit_tool_update(update, emit);
    }
}

async fn drain_steering(config: &RichAgentLoopConfig) -> Vec<AgentMessage> {
    let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (config.get_steering_messages)(())
    }));
    match future {
        Ok(future) => std::panic::AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn drain_follow_up(config: &RichAgentLoopConfig) -> Vec<AgentMessage> {
    let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (config.get_follow_up_messages)(())
    }));
    match future {
        Ok(future) => std::panic::AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn safely_await_optional<F, T>(future: F, signal: Option<Arc<AtomicBool>>) -> Option<T>
where
    F: Future<Output = Option<T>> + Send,
    T: Send,
{
    let future = std::panic::AssertUnwindSafe(future).catch_unwind();
    tokio::pin!(future);
    if let Some(signal) = signal {
        tokio::select! {
            value = &mut future => value.ok().flatten(),
            _ = wait_for_abort(signal) => None,
        }
    } else {
        future.await.ok().flatten()
    }
}

#[derive(Debug)]
enum HookFailure {
    Panicked(String),
    Aborted,
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "panic payload was not a string".to_string()
}

async fn await_hook_optional<F, T>(
    future: F,
    signal: Option<Arc<AtomicBool>>,
) -> Result<Option<T>, HookFailure>
where
    F: Future<Output = Option<T>> + Send,
    T: Send,
{
    let future = std::panic::AssertUnwindSafe(future).catch_unwind();
    tokio::pin!(future);
    if let Some(signal) = signal {
        tokio::select! {
            value = &mut future => value.map_err(|panic| HookFailure::Panicked(panic_payload_message(panic))),
            _ = wait_for_abort(signal) => Err(HookFailure::Aborted),
        }
    } else {
        future
            .await
            .map_err(|panic| HookFailure::Panicked(panic_payload_message(panic)))
    }
}

fn safe_payload_hook(hook: pi_ai::types::OnPayloadFn) -> pi_ai::types::OnPayloadFn {
    Arc::new(move |payload, model| {
        let future =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(payload, model)));
        Box::pin(async move {
            let Ok(future) = future else {
                return None;
            };
            std::panic::AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .ok()
                .flatten()
        })
    })
}

fn safe_response_hook(hook: pi_ai::model::OnResponseFn) -> pi_ai::model::OnResponseFn {
    Arc::new(move |response, model| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(response, model)));
    })
}

fn stream_options(config: &RichAgentLoopConfig) -> pi_ai::types::SimpleStreamOptions {
    let mut options = config.stream_options.clone();
    if let Some(reasoning) = config.reasoning {
        options.reasoning = Some(reasoning);
    }
    if let Some(session_id) = &config.session_id {
        options.base.session_id = Some(session_id.clone());
    }
    if config.max_retry_delay_ms.is_some() {
        options.base.base.max_retry_delay_ms = config.max_retry_delay_ms;
    }
    options.base.abort_signal = config.signal.clone();

    let api_key = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        config
            .get_api_key
            .as_ref()
            .and_then(|resolver| resolver(&config.model.provider))
    }))
    .ok()
    .flatten()
    .or_else(|| config.api_key.clone());
    if api_key.is_some() {
        options.base.base.api_key = api_key;
    }

    options.base.on_payload = config
        .on_payload
        .clone()
        .or_else(|| options.base.on_payload.clone())
        .map(safe_payload_hook);
    options.base.on_response = config
        .on_response
        .clone()
        .or_else(|| options.base.on_response.clone())
        .map(safe_response_hook);
    options
}

fn invoke_stream(
    config: &RichAgentLoopConfig,
    model: &pi_ai::model::Model,
    context: &Context,
    options: &pi_ai::types::SimpleStreamOptions,
) -> pi_ai::AssistantMessageEventStream {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(stream_fn) = &config.stream_fn_with_options {
            stream_fn(model, context, options)
        } else {
            (config.stream_fn)(model, context)
        }
    }));
    match result {
        Ok(stream) => stream,
        Err(panic) => pi_ai::create_error_stream(
            &model.api,
            &model.provider,
            &model.id,
            panic_payload_message(panic),
        ),
    }
}

async fn wait_for_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

fn overflow_recovery_case(
    message: &AssistantMessage,
    model: &pi_ai::model::Model,
) -> Option<(OverflowRecoveryReason, bool)> {
    // Provider adaptors normally stamp every response. Treat omitted
    // metadata as belonging to the current request for compatibility with
    // custom adaptors, but never cross a response explicitly stamped for a
    // different provider/model.
    if message
        .provider()
        .is_some_and(|provider| provider != model.provider)
        || message
            .model()
            .is_some_and(|response_model| response_model != model.id)
    {
        return None;
    }

    if pi_ai::utils::is_context_overflow(message, Some(model.context_window)) {
        return Some((
            OverflowRecoveryReason::ContextOverflow,
            message.stop_reason() != Some(StopReason::Stop),
        ));
    }
    if pi_ai::utils::is_recoverable_length(message, model.max_tokens) {
        return Some((OverflowRecoveryReason::RecoverableLength, true));
    }
    None
}

async fn invoke_overflow_recovery(
    hook: OverflowRecoveryHook,
    request: OverflowRecoveryRequest,
    signal: Option<Arc<AtomicBool>>,
) -> Result<OverflowRecoveryResult, String> {
    let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hook(request, signal.clone())
    }))
    .map_err(|_| "overflow recovery hook panicked".to_string())?;
    let future = std::panic::AssertUnwindSafe(future).catch_unwind();
    tokio::pin!(future);

    if let Some(signal) = signal {
        tokio::select! {
            result = &mut future => result
                .map_err(|_| "overflow recovery hook panicked".to_string())?,
            _ = wait_for_abort(signal) => Err("overflow recovery cancelled".to_string()),
        }
    } else {
        future
            .await
            .map_err(|_| "overflow recovery hook panicked".to_string())?
    }
}

fn remove_failed_response(messages: &mut Vec<AgentMessage>, failed: &AssistantMessage) {
    if let Some(index) = messages.iter().rposition(|message| {
        matches!(
            message,
            AgentMessage::Core(Message::Assistant(candidate)) if candidate == failed
        )
    }) {
        messages.remove(index);
    }
}

fn context_contains_response(messages: &[AgentMessage], response: &AssistantMessage) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            AgentMessage::Core(Message::Assistant(candidate)) if candidate == response
        )
    })
}

fn aborted_assistant_message(model: &pi_ai::model::Model) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_stop_reason(StopReason::Aborted);
    message
}

/// Fail all tool calls from an assistant message truncated by the output
/// token limit (upstream `failToolCallsFromTruncatedMessage`).
async fn fail_tool_calls_from_truncated_message<F>(
    message: &AssistantMessage,
    emit: &mut F,
) -> ExecutedToolBatch
where
    F: FnMut(RichAgentEvent) + Send,
{
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tc in tool_calls_of(message) {
        emit(RichAgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            args: tc.arguments.clone(),
        });
        let result = create_error_agent_tool_result(&format!(
            "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            tc.name
        ));
        let message_result = agent_tool_result_to_message(tc.id, tc.name, &result, true);
        emit(RichAgentEvent::ToolExecutionEnd {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            result: result.clone(),
            is_error: true,
        });
        emit_tool_result_messages(&mut messages, message_result, emit);
    }
    ExecutedToolBatch {
        messages,
        terminate: false,
    }
}

fn emit_tool_result_messages<F>(
    messages: &mut Vec<ToolResultMessage>,
    result: ToolResultMessage,
    emit: &mut F,
) where
    F: FnMut(RichAgentEvent) + Send,
{
    let m = AgentMessage::Core(Message::ToolResult(result.clone()));
    emit(RichAgentEvent::MessageStart { message: m.clone() });
    emit(RichAgentEvent::MessageEnd { message: m });
    messages.push(result);
}

/// Execute tool calls from an assistant message (upstream `executeToolCalls`
/// dispatches to sequential/parallel).
async fn execute_tool_batch<F>(
    message: &AssistantMessage,
    context: &AgentContext,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> ExecutedToolBatch
where
    F: FnMut(RichAgentEvent) + Send,
{
    let tool_calls = tool_calls_of(message);
    let has_sequential_tool = tool_calls.iter().any(|tc| {
        context
            .tools
            .iter()
            .find(|tool| tool.tool.name == tc.name)
            .and_then(|tool| tool.execution_mode)
            == Some(crate::tools::ToolExecutionMode::Sequential)
    });
    if config.tool_execution == ToolExecutionMode::Sequential
        || has_sequential_tool
        || tool_calls.len() <= 1
    {
        return execute_tool_calls_sequential(message, context, config, emit).await;
    }
    execute_tool_calls_parallel(message, context, config, emit).await
}

async fn execute_tool_calls_sequential<F>(
    message: &AssistantMessage,
    context: &AgentContext,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> ExecutedToolBatch
where
    F: FnMut(RichAgentEvent) + Send,
{
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    let mut terminate_flags: Vec<bool> = Vec::new();
    let (update_sender, mut update_receiver) =
        tokio::sync::mpsc::unbounded_channel::<ToolUpdateEvent>();
    for tc in tool_calls_of(message) {
        emit(RichAgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            args: tc.arguments.clone(),
        });
        match prepare_tool_call(message, &tc, context, config).await {
            PreparedToolCall::Immediate { result, is_error } => {
                let terminate = result.terminate;
                let message_result =
                    agent_tool_result_to_message(tc.id, tc.name, &result, is_error);
                emit(RichAgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.to_string(),
                    tool_name: tc.name.to_string(),
                    result: result.clone(),
                    is_error,
                });
                emit_tool_result_messages(&mut messages, message_result, emit);
                terminate_flags.push(terminate);
            }
            PreparedToolCall::Prepared { tool, args } => {
                let execution = start_tool_execution(
                    &tc,
                    tool,
                    args.clone(),
                    config.signal.as_ref(),
                    update_sender.clone(),
                );
                let (result, is_error) =
                    await_tool_execution(execution, &mut update_receiver, emit).await;
                let (result, is_error) =
                    finalize_tool_call(message, &tc, args, result, is_error, config).await;
                let terminate = result.terminate;
                let message_result =
                    agent_tool_result_to_message(tc.id, tc.name, &result, is_error);
                emit(RichAgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.to_string(),
                    tool_name: tc.name.to_string(),
                    result: result.clone(),
                    is_error,
                });
                emit_tool_result_messages(&mut messages, message_result, emit);
                terminate_flags.push(terminate);
            }
        }
        if is_aborted(config.signal.as_ref()) {
            break;
        }
    }
    let terminate = !terminate_flags.is_empty() && terminate_flags.iter().all(|t| *t);
    ExecutedToolBatch {
        messages,
        terminate,
    }
}

async fn execute_tool_calls_parallel<F>(
    message: &AssistantMessage,
    context: &AgentContext,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> ExecutedToolBatch
where
    F: FnMut(RichAgentEvent) + Send,
{
    // Phase 1: prepare each call sequentially (upstream preflights).
    let tool_calls = tool_calls_of(message);
    type Finalized = (String, String, crate::tools::AgentToolResult, bool);
    let mut prepared: Vec<Option<PreparedToolCall>> = (0..tool_calls.len()).map(|_| None).collect();
    let mut finalized: Vec<Option<Finalized>> = (0..tool_calls.len()).map(|_| None).collect();
    for (index, tc) in tool_calls.iter().enumerate() {
        emit(RichAgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            args: tc.arguments.clone(),
        });
        match prepare_tool_call(message, tc, context, config).await {
            PreparedToolCall::Immediate { result, is_error } => {
                let id = tc.id.to_string();
                let name = tc.name.to_string();
                emit(RichAgentEvent::ToolExecutionEnd {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    result: result.clone(),
                    is_error,
                });
                finalized[index] = Some((id, name, result, is_error));
            }
            preparation @ PreparedToolCall::Prepared { .. } => {
                prepared[index] = Some(preparation);
            }
        }
        if is_aborted(config.signal.as_ref()) {
            break;
        }
    }

    // Phase 2: execute prepared calls concurrently. End events are emitted as
    // each call is finalized, while result-message artifacts are emitted in
    // assistant source order below (upstream `executeToolCallsParallel`).
    let signal = config.signal.clone();

    let (update_sender, mut update_receiver) =
        tokio::sync::mpsc::unbounded_channel::<ToolUpdateEvent>();
    let mut running = futures_util::stream::FuturesUnordered::new();

    for (index, tc) in tool_calls.iter().enumerate() {
        let Some(preparation) = prepared.get_mut(index).and_then(Option::take) else {
            continue;
        };
        match preparation {
            PreparedToolCall::Prepared { tool, args } => {
                let id = tc.id.to_string();
                let name = tc.name.to_string();
                let raw_args = tc.arguments.clone();
                let assistant_message = message.clone();
                let update_sender = update_sender.clone();
                let signal = signal.clone();
                running.push(
                    async move {
                        let tool_call = ToolCallRef {
                            id: &id,
                            name: &name,
                            arguments: &raw_args,
                        };
                        let (result, is_error) = start_tool_execution(
                            &tool_call,
                            tool,
                            args.clone(),
                            signal.as_ref(),
                            update_sender,
                        )
                        .await;
                        let (result, is_error) = finalize_tool_call(
                            &assistant_message,
                            &tool_call,
                            args,
                            result,
                            is_error,
                            config,
                        )
                        .await;
                        (index, id, name, result, is_error)
                    }
                    .boxed(),
                );
            }
            PreparedToolCall::Immediate { .. } => unreachable!(
                "immediate tool calls are emitted during the parallel preparation phase"
            ),
        }
    }

    drop(update_sender);
    let mut updates_open = true;
    while !running.is_empty() {
        tokio::select! {
            update = update_receiver.recv(), if updates_open => {
                match update {
                    Some(update) => emit_tool_update(update, emit),
                    None => updates_open = false,
                }
            }
            output = running.next() => {
                if let Some((index, id, name, result, is_error)) = output {
                    // A tool can enqueue its final update immediately before
                    // resolving. Drain that queue before the matching end
                    // event so observers never see tool completion before the
                    // last update that caused it.
                    drain_queued_tool_updates(&mut update_receiver, emit);
                    emit(RichAgentEvent::ToolExecutionEnd {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        result: result.clone(),
                        is_error,
                    });
                    finalized[index] = Some((id, name, result, is_error));
                }
            }
        }
    }
    drain_queued_tool_updates(&mut update_receiver, emit);

    let mut messages: Vec<ToolResultMessage> = Vec::new();
    let mut terminate_flags: Vec<bool> = Vec::new();
    // Keep this pass separate from completion event emission above: result
    // messages are the durable/model-facing artifacts and retain assistant
    // source order even when execution completes out of order.
    for entry in finalized.into_iter().flatten() {
        let (id, name, result, is_error) = entry;
        let message_result = agent_tool_result_to_message(&id, &name, &result, is_error);
        emit_tool_result_messages(&mut messages, message_result, emit);
        terminate_flags.push(result.terminate);
    }
    let terminate = !terminate_flags.is_empty() && terminate_flags.iter().all(|t| *t);
    ExecutedToolBatch {
        messages,
        terminate,
    }
}

enum PreparedToolCall {
    Prepared {
        tool: AgentTool,
        args: serde_json::Value,
    },
    Immediate {
        result: crate::tools::AgentToolResult,
        is_error: bool,
    },
}

/// Resolve + validate + beforeToolCall for one tool call (upstream
/// `prepareToolCall`).
async fn prepare_tool_call(
    message: &AssistantMessage,
    tc: &ToolCallRef<'_>,
    context: &AgentContext,
    config: &RichAgentLoopConfig,
) -> PreparedToolCall {
    let Some(tool) = context.tools.iter().find(|t| t.tool.name == tc.name) else {
        return PreparedToolCall::Immediate {
            result: create_error_agent_tool_result(&format!("Tool {} not found", tc.name)),
            is_error: true,
        };
    };
    // prepareArguments compatibility shim (upstream `prepareToolCallArguments`).
    let mut args = tc.arguments.clone();
    if let Some(prepare) = &tool.prepare_arguments {
        let prepared = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            prepare(args.clone())
        })) {
            Ok(prepared) => prepared,
            Err(panic) => {
                return PreparedToolCall::Immediate {
                    result: create_error_agent_tool_result(&panic_payload_message(panic)),
                    is_error: true,
                };
            }
        };
        if prepared != args {
            args = prepared;
        }
    }
    // validateToolArguments: schema validation with coercion (upstream
    // `validateToolArguments`). Failures become immediate error results.
    let mut validated = match crate::tools::validation::validate_tool_arguments(
        tc.name,
        &tool.tool.parameters,
        &args,
    ) {
        Ok(validated) => validated,
        Err(e) => {
            return PreparedToolCall::Immediate {
                result: create_error_agent_tool_result(&e),
                is_error: true,
            };
        }
    };
    if let Some(hook) = &config.before_tool_call {
        let mut before_context = BeforeToolCallContext {
            assistant_message: message.clone(),
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            args: validated.clone(),
        };
        let before_future =
            match invoke_before_tool_call(hook, &mut before_context, config.signal.clone()) {
                Ok(future) => future,
                Err(panic) => {
                    return PreparedToolCall::Immediate {
                        result: create_error_agent_tool_result(&panic_payload_message(panic)),
                        is_error: true,
                    };
                }
            };
        let before = match await_hook_optional(before_future, config.signal.clone()).await {
            Ok(before) => before,
            Err(HookFailure::Aborted) => {
                return PreparedToolCall::Immediate {
                    result: create_error_agent_tool_result("Operation aborted"),
                    is_error: true,
                };
            }
            Err(HookFailure::Panicked(message)) => {
                return PreparedToolCall::Immediate {
                    result: create_error_agent_tool_result(&message),
                    is_error: true,
                };
            }
        };
        validated = before_context.args;
        if is_aborted(config.signal.as_ref()) {
            return PreparedToolCall::Immediate {
                result: create_error_agent_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(before) = before {
            if before.block {
                let reason = before
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".to_string());
                let mut result = create_error_agent_tool_result(&reason);
                result.terminate = before.terminate;
                return PreparedToolCall::Immediate {
                    result,
                    is_error: true,
                };
            }
        }
    }
    if is_aborted(config.signal.as_ref()) {
        return PreparedToolCall::Immediate {
            result: create_error_agent_tool_result("Operation aborted"),
            is_error: true,
        };
    }
    PreparedToolCall::Prepared {
        tool: tool.clone(),
        args: validated,
    }
}

type ToolExecutionFuture =
    Pin<Box<dyn Future<Output = (crate::tools::AgentToolResult, bool)> + Send>>;

fn start_tool_execution(
    tc: &ToolCallRef<'_>,
    tool: AgentTool,
    args: serde_json::Value,
    signal: Option<&Arc<AtomicBool>>,
    update_sender: tokio::sync::mpsc::UnboundedSender<ToolUpdateEvent>,
) -> ToolExecutionFuture {
    let tool_call_id = tc.id.to_string();
    let tool_name = tc.name.to_string();
    let update_args = tc.arguments.clone();
    let signal = signal.cloned();
    let accepting_updates = Arc::new(AtomicBool::new(true));
    let update_gate = accepting_updates.clone();
    let update_call_id = tool_call_id.clone();
    let on_update: crate::tools::ToolUpdateCallback = Arc::new(move |partial_result| {
        if !update_gate.load(Ordering::Acquire) {
            return;
        }
        let _ = update_sender.send(ToolUpdateEvent {
            tool_call_id: update_call_id.clone(),
            tool_name: tool_name.clone(),
            args: update_args.clone(),
            partial_result: partial_result.clone(),
        });
    });
    Box::pin(async move {
        if is_aborted(signal.as_ref()) {
            accepting_updates.store(false, Ordering::Release);
            return (
                crate::tools::AgentToolResult::text("Operation aborted"),
                true,
            );
        }
        let abort_signal = signal.clone();
        let mut execution = Box::pin(async move {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (tool.execute)(tool_call_id, args, signal.clone(), Some(on_update))
            })) {
                Ok(future) => std::panic::AssertUnwindSafe(future)
                    .catch_unwind()
                    .await
                    .map_err(panic_payload_message),
                Err(panic) => Err(panic_payload_message(panic)),
            }
        });
        let execution = if let Some(signal) = abort_signal {
            tokio::select! {
                result = &mut execution => result,
                _ = wait_for_abort(signal) => {
                    accepting_updates.store(false, Ordering::Release);
                    // The cancellation branch must not strand a cooperative
                    // tool's cleanup (for example the shell process-group
                    // kill) merely because the loop stopped waiting for it.
                    // The update gate prevents late progress from becoming a
                    // post-settlement lifecycle event.
                    tokio::spawn(async move {
                        let _ = execution.await;
                    });
                    return (
                        crate::tools::AgentToolResult::text("Operation aborted"),
                        true,
                    );
                }
            }
        } else {
            execution.await
        };
        accepting_updates.store(false, Ordering::Release);
        match execution {
            Ok(Ok(result)) => (result, false),
            Ok(Err(e)) => (crate::tools::AgentToolResult::text(e), true),
            Err(e) => (crate::tools::AgentToolResult::text(e), true),
        }
    })
}

async fn await_tool_execution<F>(
    mut execution: ToolExecutionFuture,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<ToolUpdateEvent>,
    emit: &mut F,
) -> (crate::tools::AgentToolResult, bool)
where
    F: FnMut(RichAgentEvent) + Send,
{
    let mut updates_open = true;
    let result = loop {
        tokio::select! {
            update = receiver.recv(), if updates_open => {
                match update {
                    Some(update) => emit_tool_update(update, emit),
                    None => updates_open = false,
                }
            }
            result = &mut execution => break result,
        }
    };
    drain_queued_tool_updates(receiver, emit);
    result
}

/// afterToolCall merge (upstream `finalizeExecutedToolCall`).
async fn finalize_tool_call(
    message: &AssistantMessage,
    tc: &ToolCallRef<'_>,
    args: serde_json::Value,
    mut result: crate::tools::AgentToolResult,
    is_error: bool,
    config: &RichAgentLoopConfig,
) -> (crate::tools::AgentToolResult, bool) {
    let (mut result, is_error) = if let Some(hook) = &config.after_tool_call {
        let after_future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook(
                AfterToolCallContext {
                    assistant_message: message.clone(),
                    tool_call_id: tc.id.to_string(),
                    tool_name: tc.name.to_string(),
                    args,
                    result: agent_tool_result_to_message(tc.id, tc.name, &result, is_error),
                    is_error,
                },
                config.signal.clone(),
            )
        }));
        let after = match after_future {
            Ok(future) => match await_hook_optional(future, config.signal.clone()).await {
                Ok(after) => after,
                Err(HookFailure::Aborted) => {
                    return (create_error_agent_tool_result("Operation aborted"), true);
                }
                Err(HookFailure::Panicked(message)) => {
                    return (create_error_agent_tool_result(&message), true);
                }
            },
            Err(panic) => {
                let message = panic_payload_message(panic);
                return (create_error_agent_tool_result(&message), true);
            }
        };
        if let Some(after) = after {
            if let Some(content) = after.content {
                result.content = content;
            }
            if let Some(details) = after.details {
                result.details = Some(details);
            }
            if let Some(usage) = after.usage {
                result.usage = Some(usage);
            }
            if let Some(added_tool_names) = after.added_tool_names {
                result.added_tool_names = added_tool_names;
            }
            if let Some(terminate) = after.terminate {
                result.terminate = terminate;
            }
            (result, after.is_error.unwrap_or(is_error))
        } else {
            (result, is_error)
        }
    } else {
        (result, is_error)
    };
    if let Some(options) = config.tool_result_image_options {
        result.content =
            crate::tools::image::normalize_tool_result_images(&result.content, options);
    }
    (result, is_error)
}

/// Run the rich agent loop (upstream `runAgentLoop`).
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
pub async fn run_rich_agent_loop<F>(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> Vec<AgentMessage>
where
    F: FnMut(RichAgentEvent) + Send,
{
    let mut current_config = config.clone();
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_messages: Vec<AgentMessage> = context
        .messages
        .iter()
        .cloned()
        .chain(prompts.clone())
        .collect();
    context.messages = current_messages.clone();

    emit(RichAgentEvent::AgentStart);
    emit(RichAgentEvent::TurnStart);
    for prompt in &prompts {
        emit(RichAgentEvent::MessageStart {
            message: prompt.clone(),
        });
        emit(RichAgentEvent::MessageEnd {
            message: prompt.clone(),
        });
    }

    let mut first_turn = false;
    let mut pending_messages: Vec<AgentMessage> = drain_steering(&current_config).await;
    // Upstream resets this guard when a new user message starts. It is kept
    // local to one loop invocation so an overflow can never spin forever.
    let mut overflow_recovery_attempted = false;

    'outer: loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending_messages.is_empty() {
            if first_turn {
                emit(RichAgentEvent::TurnStart);
            } else {
                first_turn = true;
            }

            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    if message.role() == "user" {
                        overflow_recovery_attempted = false;
                    }
                    emit(RichAgentEvent::MessageStart {
                        message: message.clone(),
                    });
                    emit(RichAgentEvent::MessageEnd {
                        message: message.clone(),
                    });
                    current_messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message =
                stream_assistant_response(&current_messages, context, &current_config, emit).await;
            let assistant_message = AgentMessage::Core(Message::Assistant(message.clone()));
            current_messages.push(assistant_message.clone());
            context.messages = current_messages.clone();
            new_messages.push(assistant_message.clone());

            // A successful assistant turn starts a fresh opportunity for a
            // later tool-driven turn to recover. Error and length responses
            // retain the guard so a single interrupted turn cannot recurse.
            if !matches!(
                message.stop_reason(),
                Some(StopReason::Error) | Some(StopReason::Length)
            ) {
                overflow_recovery_attempted = false;
            }

            // Automatic compaction is intentionally a callback boundary. The
            // session owner supplies the real durable append and summarizing
            // provider; the shared loop only performs classification, bounded
            // control flow, and failed-response context hygiene.
            if let Some((reason, will_retry)) =
                overflow_recovery_case(&message, &current_config.model)
            {
                let can_retry = will_retry
                    && !overflow_recovery_attempted
                    && current_config.overflow_recovery.is_some();
                let should_compact_without_retry =
                    !will_retry && current_config.overflow_recovery.is_some();
                if can_retry || should_compact_without_retry {
                    if can_retry {
                        overflow_recovery_attempted = true;
                    }
                    let retry_messages = current_messages[..current_messages.len() - 1].to_vec();
                    let request = OverflowRecoveryRequest {
                        reason,
                        assistant_message: message.clone(),
                        model: current_config.model.clone(),
                        context: context.clone(),
                        durable_messages: current_messages.clone(),
                        retry_messages,
                        will_retry,
                    };
                    let recovery = invoke_overflow_recovery(
                        current_config
                            .overflow_recovery
                            .as_ref()
                            .expect("overflow recovery hook checked above")
                            .clone(),
                        request,
                        current_config.signal.clone(),
                    )
                    .await;
                    if let Ok(mut recovered) = recovery {
                        if will_retry {
                            remove_failed_response(&mut recovered.context.messages, &message);
                        } else if !context_contains_response(&recovered.context.messages, &message)
                        {
                            // A successful overflow is compacted without a
                            // retry and must remain available to the active
                            // session even if a caller's rebuild omitted it.
                            recovered.context.messages.push(assistant_message.clone());
                        }
                        *context = recovered.context;
                        current_messages = context.messages.clone();
                        if will_retry {
                            emit(RichAgentEvent::TurnEnd {
                                message: assistant_message.clone(),
                                tool_results: vec![],
                            });
                            continue;
                        }
                    }
                }
            }

            if matches!(
                message.stop_reason(),
                Some(StopReason::Error) | Some(StopReason::Aborted)
            ) {
                emit(RichAgentEvent::TurnEnd {
                    message: AgentMessage::Core(Message::Assistant(message)),
                    tool_results: vec![],
                });
                emit(RichAgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                context.messages = current_messages.clone();
                return new_messages;
            }

            let tool_calls: Vec<pi_ai::types::ContentBlock> = message
                .content()
                .iter()
                .filter(|c| matches!(c, pi_ai::types::ContentBlock::ToolCall { .. }))
                .cloned()
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let truncated = message.stop_reason() == Some(StopReason::Length);
                let batch = if truncated {
                    fail_tool_calls_from_truncated_message(&message, emit).await
                } else {
                    execute_tool_batch(&message, context, &current_config, emit).await
                };
                tool_results.extend(batch.messages);
                // A non-terminating tool batch must cause another assistant
                // turn. Upstream's `hasMoreToolCalls` is the loop gate; the
                // earlier Rust port left it false for every successful batch,
                // which dropped the follow-up model response after tools.
                has_more_tool_calls = !batch.terminate;
                for result in &tool_results {
                    current_messages.push(AgentMessage::Core(Message::ToolResult(result.clone())));
                    new_messages.push(AgentMessage::Core(Message::ToolResult(result.clone())));
                }
                context.messages = current_messages.clone();
            }

            emit(RichAgentEvent::TurnEnd {
                message: AgentMessage::Core(Message::Assistant(message.clone())),
                tool_results: tool_results.clone(),
            });

            // Upstream invokes next-turn preparation after `turn_end` and
            // before checking whether another provider request should start.
            // The context snapshot includes the completed assistant/tool
            // messages, even though the durable caller context is separately
            // maintained by the stateful Agent wrapper.
            let next_turn_context = PrepareNextTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: {
                    let mut snapshot = context.clone();
                    snapshot.messages = current_messages.clone();
                    snapshot
                },
                new_messages: new_messages.clone(),
            };
            let next_turn = if let Some(hook) = &current_config.prepare_next_turn_with_context {
                let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    hook(next_turn_context, current_config.signal.clone())
                }));
                match future {
                    Ok(future) => {
                        safely_await_optional(future, current_config.signal.clone()).await
                    }
                    Err(_) => None,
                }
            } else if let Some(hook) = &current_config.prepare_next_turn {
                let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    hook(current_config.signal.clone())
                }));
                match future {
                    Ok(future) => {
                        safely_await_optional(future, current_config.signal.clone()).await
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            if let Some(next_turn) = next_turn {
                if let Some(next_context) = next_turn.context {
                    *context = next_context;
                    current_messages = context.messages.clone();
                }
                if let Some(model) = next_turn.model {
                    current_config.model = model;
                }
                if let Some(thinking_level) = next_turn.thinking_level {
                    current_config.reasoning = thinking_level;
                    current_config.stream_options.reasoning = thinking_level;
                }
                if let Some(options) = next_turn.stream_options {
                    current_config.session_id = options.base.session_id.clone();
                    current_config.reasoning = options.reasoning;
                    current_config.stream_options = options;
                }
            }

            let should_stop = match &current_config.should_stop_after_turn {
                Some(hook) => std::panic::AssertUnwindSafe(hook(message, tool_results.clone()))
                    .catch_unwind()
                    .await
                    .unwrap_or(false),
                None => false,
            };
            if should_stop {
                emit(RichAgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                context.messages = current_messages.clone();
                return new_messages;
            }

            pending_messages = drain_steering(&current_config).await;
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_ups = drain_follow_up(&current_config).await;
        if !follow_ups.is_empty() {
            pending_messages = follow_ups;
            continue 'outer;
        }
        break;
    }

    emit(RichAgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
    context.messages = current_messages;
    new_messages
}

/// Stream an assistant response with full event emission (upstream
/// `streamAssistantResponse`).
async fn stream_assistant_response<F>(
    current_messages: &[AgentMessage],
    context: &AgentContext,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> AssistantMessage
where
    F: FnMut(RichAgentEvent) + Send,
{
    if is_aborted(config.signal.as_ref()) {
        return aborted_assistant_message(&config.model);
    }

    // Apply context transform if configured (AgentMessage[] -> AgentMessage[]).
    let mut messages = current_messages.to_vec();
    if let Some(transform) = &config.transform_context {
        let fallback = messages.clone();
        messages = std::panic::AssertUnwindSafe(transform((messages, config.signal.clone())))
            .catch_unwind()
            .await
            .unwrap_or(fallback);
    }

    // Convert to LLM-compatible messages.
    let mut llm_messages: Vec<Message> = match &config.convert_to_llm {
        Some(convert) => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| convert(&messages)))
                .unwrap_or_else(|_| crate::messages::convert_to_llm(&messages))
        }
        None => crate::messages::convert_to_llm(&messages),
    };
    if config.block_images {
        llm_messages = crate::messages::filter_images_for_provider(llm_messages);
    }

    let llm_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
        tools: context.tools.iter().map(|t| t.tool.clone()).collect(),
    };

    let request_options = stream_options(config);

    if let Some(policy) = &config.retry_policy {
        // Retry is implemented at the assistant-call boundary, but every
        // attempt still follows the normal rich message lifecycle. Keep the
        // events in a shared ordered buffer while the retry helper sleeps and
        // invokes later attempts; the caller can then emit the complete
        // attempt history in the same order it was observed.
        let retry_events = Arc::new(Mutex::new(Vec::<RichAgentEvent>::new()));
        let retry_config = config.clone();
        let model = config.model.clone();
        let retry_context = llm_context.clone();
        let retry_options = request_options.clone();
        let retry_signal = config
            .retry_signal
            .clone()
            .or_else(|| config.signal.clone());
        let attempt_signal = config.signal.clone();
        let scheduled_events = retry_events.clone();
        let finished_events = retry_events.clone();
        let callback_signal = retry_signal.clone();
        let callback_run_signal = config.signal.clone();
        let callbacks = pi_ai::utils::retry::RetryCallbacks {
            on_retry_scheduled: Some(Box::new(move |attempt, max_attempts, delay_ms, error| {
                scheduled_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(RichAgentEvent::AutoRetryStart {
                        attempt,
                        max_attempts,
                        delay_ms,
                        error_message: error,
                    });
            })),
            on_retry_finished: Some(Box::new(move |success, attempt, final_error| {
                let final_error = if !success
                    && callback_signal
                        .as_ref()
                        .is_some_and(|signal| signal.load(Ordering::SeqCst))
                    && !callback_run_signal
                        .as_ref()
                        .is_some_and(|signal| signal.load(Ordering::SeqCst))
                {
                    Some("Retry cancelled".to_string())
                } else {
                    final_error
                };
                finished_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(RichAgentEvent::AutoRetryEnd {
                        success,
                        attempt,
                        final_error,
                    });
            })),
            ..Default::default()
        };
        let producer_events = retry_events.clone();
        let mut final_message = pi_ai::utils::retry::retry_assistant_call(
            move || {
                let stream = invoke_stream(&retry_config, &model, &retry_context, &retry_options);
                let signal = attempt_signal.clone();
                let producer_events = producer_events.clone();
                let attempt_model = model.clone();
                async move {
                    stream_assistant_attempt(stream, signal, producer_events, attempt_model).await
                }
            },
            Some(policy),
            retry_signal.as_ref(),
            Some(&callbacks),
        )
        .await;

        // Cancelling only the retry backoff leaves the original provider
        // error as the authoritative final response. A normal run abort (or
        // a provider abort on a later attempt) remains an aborted response.
        let retry_was_cancelled = final_message.stop_reason() == Some(StopReason::Aborted)
            && !is_aborted(config.signal.as_ref())
            && retry_signal
                .as_ref()
                .is_some_and(|signal| signal.load(Ordering::SeqCst));
        if retry_was_cancelled {
            if let Some(error) = retry_events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .rev()
                .find_map(|event| {
                    let RichAgentEvent::MessageEnd { message } = event else {
                        return None;
                    };
                    let AgentMessage::Core(Message::Assistant(message)) = message else {
                        return None;
                    };
                    (message.stop_reason() == Some(StopReason::Error)).then(|| message.clone())
                })
            {
                final_message = error;
            }
        } else if is_aborted(config.signal.as_ref())
            && !retry_events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .any(|event| {
                    matches!(
                        event,
                        RichAgentEvent::MessageEnd {
                            message: AgentMessage::Core(Message::Assistant(message))
                        } if message.stop_reason() == Some(StopReason::Aborted)
                    )
                })
        {
            // The retry helper can abort while sleeping, before it has a
            // provider stream to observe. Preserve an explicit aborted
            // terminal message for a user abort in that case.
            retry_events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(RichAgentEvent::MessageEnd {
                    message: AgentMessage::Core(Message::Assistant(final_message.clone())),
                });
        }

        let buffered = std::mem::take(
            &mut *retry_events
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for event in buffered {
            emit(event);
        }
        if is_aborted(config.signal.as_ref()) {
            final_message.set_stop_reason(StopReason::Aborted);
        }
        return final_message;
    }

    let stream = invoke_stream(config, &config.model, &llm_context, &request_options);

    let emit_ref: &mut (dyn FnMut(RichAgentEvent) + Send) = emit;
    let mut added_partial = false;
    let mut stream_future = Box::pin(stream.for_each(|event| match &event {
        AssistantMessageEvent::Start { partial } => {
            added_partial = true;
            emit_ref(RichAgentEvent::MessageStart {
                message: AgentMessage::Core(Message::Assistant(partial.clone())),
            });
        }
        AssistantMessageEvent::TextStart { .. }
        | AssistantMessageEvent::TextDelta { .. }
        | AssistantMessageEvent::TextEnd { .. }
        | AssistantMessageEvent::ThinkingStart { .. }
        | AssistantMessageEvent::ThinkingDelta { .. }
        | AssistantMessageEvent::ThinkingEnd { .. }
        | AssistantMessageEvent::ToolCallStart { .. }
        | AssistantMessageEvent::ToolCallDelta { .. }
        | AssistantMessageEvent::ToolCallEnd { .. } => {
            if let Some(partial) = event.partial() {
                emit_ref(RichAgentEvent::MessageUpdate {
                    message: AgentMessage::Core(Message::Assistant(partial.clone())),
                    assistant_message_event: event.clone(),
                });
            }
        }
        AssistantMessageEvent::Done { .. } => {}
        AssistantMessageEvent::Error {
            error_message: message,
            ..
        } => {
            // Raw stream observers see terminal events too. Preserve
            // them in the rich stream so JSON/RPC callers do not lose a
            // provider error when they run through this loop.
            emit_ref(RichAgentEvent::MessageUpdate {
                message: AgentMessage::Core(Message::Assistant(message.clone())),
                assistant_message_event: event.clone(),
            });
        }
    }));
    let (mut final_message, aborted) = if let Some(signal) = config.signal.clone() {
        tokio::select! {
            message = &mut stream_future => (message, false),
            _ = wait_for_abort(signal) => {
                (aborted_assistant_message(&config.model), true)
            }
        }
    } else {
        ((&mut stream_future).await, false)
    };
    drop(stream_future);
    if aborted || is_aborted(config.signal.as_ref()) {
        final_message.set_stop_reason(StopReason::Aborted);
    }

    // Providers are allowed to return a terminal event without a start event.
    // The agent lifecycle still requires message_start before message_end. Do
    // not duplicate it when a partial start was already observed.
    if !added_partial {
        emit(RichAgentEvent::MessageStart {
            message: AgentMessage::Core(Message::Assistant(final_message.clone())),
        });
    }
    emit(RichAgentEvent::MessageEnd {
        message: AgentMessage::Core(Message::Assistant(final_message.clone())),
    });
    final_message
}

/// Consume one provider stream while recording the same rich events as the
/// non-retry path. The retry helper owns only attempt selection/backoff; this
/// function owns stream observation and the attempt's final message lifecycle.
async fn stream_assistant_attempt(
    stream: pi_ai::AssistantMessageEventStream,
    signal: Option<Arc<AtomicBool>>,
    events: Arc<Mutex<Vec<RichAgentEvent>>>,
    model: pi_ai::model::Model,
) -> AssistantMessage {
    let mut added_partial = false;
    let events_for_stream = events.clone();
    let mut stream_future = Box::pin(stream.for_each(|event| match &event {
        AssistantMessageEvent::Start { partial } => {
            added_partial = true;
            events_for_stream
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(RichAgentEvent::MessageStart {
                    message: AgentMessage::Core(Message::Assistant(partial.clone())),
                });
        }
        AssistantMessageEvent::TextStart { .. }
        | AssistantMessageEvent::TextDelta { .. }
        | AssistantMessageEvent::TextEnd { .. }
        | AssistantMessageEvent::ThinkingStart { .. }
        | AssistantMessageEvent::ThinkingDelta { .. }
        | AssistantMessageEvent::ThinkingEnd { .. }
        | AssistantMessageEvent::ToolCallStart { .. }
        | AssistantMessageEvent::ToolCallDelta { .. }
        | AssistantMessageEvent::ToolCallEnd { .. } => {
            if let Some(partial) = event.partial() {
                events_for_stream
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(RichAgentEvent::MessageUpdate {
                        message: AgentMessage::Core(Message::Assistant(partial.clone())),
                        assistant_message_event: event.clone(),
                    });
            }
        }
        AssistantMessageEvent::Done { .. } => {}
        AssistantMessageEvent::Error {
            error_message: message,
            ..
        } => {
            events_for_stream
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(RichAgentEvent::MessageUpdate {
                    message: AgentMessage::Core(Message::Assistant(message.clone())),
                    assistant_message_event: event.clone(),
                });
        }
    }));
    let signal_for_result = signal.clone();
    let (mut final_message, aborted) = if let Some(signal) = signal {
        tokio::select! {
            message = &mut stream_future => (message, false),
            _ = wait_for_abort(signal) => (aborted_assistant_message(&model), true),
        }
    } else {
        ((&mut stream_future).await, false)
    };
    drop(stream_future);

    if aborted || is_aborted(signal_for_result.as_ref()) {
        final_message.set_stop_reason(StopReason::Aborted);
    }
    let final_agent_message = AgentMessage::Core(Message::Assistant(final_message.clone()));
    if !added_partial {
        events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(RichAgentEvent::MessageStart {
                message: final_agent_message.clone(),
            });
    }
    events
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(RichAgentEvent::MessageEnd {
            message: final_agent_message,
        });
    final_message
}

// ===========================================================================
// Agent class — port of `agent.ts` (`Agent`)
// ===========================================================================

/// Public agent state (upstream `AgentState`).
pub struct AgentState {
    pub system_prompt: String,
    pub model: pi_ai::model::Model,
    /// Reasoning level; `None` means "off" (upstream `ThinkingLevel` union).
    pub thinking_level: Option<ThinkingLevel>,
    /// True from run admission until the final subscriber settles.
    pub is_streaming: bool,
    /// The message currently being streamed, if any.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call IDs whose execution has started but not ended.
    pub pending_tool_calls: Vec<String>,
    /// Most recent assistant error exposed by a completed turn.
    pub error_message: Option<String>,
    tools: Vec<AgentTool>,
    messages: Vec<AgentMessage>,
}

impl AgentState {
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }
    pub fn set_tools(&mut self, tools: Vec<AgentTool>) {
        self.tools = tools;
    }
    pub fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }
    pub fn set_messages(&mut self, messages: Vec<AgentMessage>) {
        self.messages = messages;
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: default_model(),
            thinking_level: None,
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: Vec::new(),
            error_message: None,
            tools: Vec::new(),
            messages: Vec::new(),
        }
    }
}

fn default_model() -> pi_ai::model::Model {
    pi_ai::model::Model {
        id: "unknown".into(),
        name: "unknown".into(),
        api: "unknown".into(),
        provider: "unknown".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: Default::default(),
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        headers: None,
        compat: None,
        authenticated: false,
        extra: Default::default(),
    }
}

/// Stateful wrapper around the low-level agent loop (upstream `Agent`).
pub struct Agent {
    state: Arc<Mutex<AgentState>>,
    listeners: Arc<Mutex<Vec<(usize, AgentListener)>>>,
    next_listener_id: AtomicUsize,
    steering_queue: Arc<Mutex<PendingMessageQueue>>,
    follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    active_run: Arc<Mutex<Option<ActiveRun>>>,
    convert_to_llm: Option<ConvertToLlmFn>,
    stream_fn: StreamFn,
    stream_fn_with_options: Option<StreamFnWithOptions>,
    on_payload: Option<pi_ai::types::OnPayloadFn>,
    on_response: Option<pi_ai::model::OnResponseFn>,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
    should_stop_after_turn: Option<ShouldStopAfterTurnHook>,
    tool_execution: ToolExecutionMode,
    block_images: bool,
    tool_result_image_options: Option<crate::tools::image::ProcessImageOptions>,
    session_id: Option<String>,
    runtime_options: Mutex<AgentRuntimeOptions>,
    prepare_next_turn: Option<PrepareNextTurnHook>,
    prepare_next_turn_with_context: Option<PrepareNextTurnWithContextHook>,
    overflow_recovery: Option<OverflowRecoveryHook>,
}

#[derive(Clone, Default)]
struct AgentRuntimeOptions {
    stream_options: pi_ai::types::SimpleStreamOptions,
    retry_policy: Option<pi_ai::utils::retry::RetryPolicy>,
    max_retry_delay_ms: Option<u64>,
    get_api_key: Option<ApiKeyResolver>,
    api_key: Option<String>,
}

struct ActiveRun {
    abort: Arc<AtomicBool>,
    done: Arc<tokio::sync::Notify>,
}

struct RunLease {
    active_run: Arc<Mutex<Option<ActiveRun>>>,
    done: Arc<tokio::sync::Notify>,
    state: Arc<Mutex<AgentState>>,
}

impl Drop for RunLease {
    fn drop(&mut self) {
        let finished = {
            let mut active = self
                .active_run
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if active
                .as_ref()
                .is_some_and(|run| Arc::ptr_eq(&run.done, &self.done))
            {
                active.take();
                true
            } else {
                false
            }
        };
        if finished {
            if let Ok(mut state) = self.state.lock() {
                state.is_streaming = false;
                state.streaming_message = None;
                state.pending_tool_calls.clear();
            }
            self.done.notify_waiters();
        }
    }
}

impl Agent {
    pub fn new(stream_fn: StreamFn) -> Self {
        Self {
            state: Arc::new(Mutex::new(AgentState::default())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicUsize::new(0),
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            active_run: Arc::new(Mutex::new(None)),
            convert_to_llm: None,
            stream_fn,
            stream_fn_with_options: None,
            on_payload: None,
            on_response: None,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            tool_execution: ToolExecutionMode::Parallel,
            block_images: false,
            tool_result_image_options: None,
            session_id: None,
            runtime_options: Mutex::new(AgentRuntimeOptions::default()),
            prepare_next_turn: None,
            prepare_next_turn_with_context: None,
            overflow_recovery: None,
        }
    }

    /// Construct an agent with the upstream-shaped option-aware stream
    /// function. [`Agent::new`] remains available for legacy two-argument
    /// stream functions.
    pub fn new_with_options(stream_fn: StreamFnWithOptions) -> Self {
        let legacy_stream: StreamFn = Arc::new(|model, _context| {
            pi_ai::create_error_stream(
                &model.api,
                &model.provider,
                &model.id,
                "an option-aware stream function is required".to_string(),
            )
        });
        let mut agent = Self::new(legacy_stream);
        agent.stream_fn_with_options = Some(stream_fn);
        agent
    }

    pub fn set_convert_to_llm(&mut self, f: ConvertToLlmFn) {
        self.convert_to_llm = Some(f);
    }
    pub fn set_stream_fn_with_options(&mut self, f: StreamFnWithOptions) {
        self.stream_fn_with_options = Some(f);
    }
    pub fn set_on_payload(&mut self, hook: pi_ai::types::OnPayloadFn) {
        self.on_payload = Some(hook);
    }
    pub fn set_on_response(&mut self, hook: pi_ai::model::OnResponseFn) {
        self.on_response = Some(hook);
    }
    pub fn set_before_tool_call(&mut self, h: BeforeToolCallHook) {
        self.before_tool_call = Some(h);
    }
    pub fn set_after_tool_call(&mut self, h: AfterToolCallHook) {
        self.after_tool_call = Some(h);
    }
    pub fn set_should_stop_after_turn(&mut self, h: ShouldStopAfterTurnHook) {
        self.should_stop_after_turn = Some(h);
    }
    pub fn set_prepare_next_turn(&mut self, h: PrepareNextTurnHook) {
        self.prepare_next_turn = Some(h);
    }
    pub fn set_prepare_next_turn_with_context(&mut self, h: PrepareNextTurnWithContextHook) {
        self.prepare_next_turn_with_context = Some(h);
    }
    /// Connect a real durable-session compaction implementation to overflow
    /// recovery. The callback is never synthesized by the agent runtime.
    pub fn set_overflow_recovery(&mut self, h: OverflowRecoveryHook) {
        self.overflow_recovery = Some(h);
    }
    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id;
    }
    pub fn set_reasoning(&mut self, level: Option<ThinkingLevel>) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .thinking_level = level;
    }
    pub fn set_stream_options(&self, options: pi_ai::types::SimpleStreamOptions) {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stream_options = options;
    }
    pub fn stream_options(&self) -> pi_ai::types::SimpleStreamOptions {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stream_options
            .clone()
    }
    pub fn set_retry_policy(&self, policy: Option<pi_ai::utils::retry::RetryPolicy>) {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retry_policy = policy;
    }
    pub fn retry_policy(&self) -> Option<pi_ai::utils::retry::RetryPolicy> {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retry_policy
            .clone()
    }
    pub fn set_max_retry_delay_ms(&self, delay_ms: Option<u64>) {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .max_retry_delay_ms = delay_ms;
    }
    pub fn set_api_key(&self, api_key: Option<String>) {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .api_key = api_key;
    }
    pub fn set_api_key_resolver(&self, resolver: Option<ApiKeyResolver>) {
        self.runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_api_key = resolver;
    }
    pub fn set_tool_execution(&mut self, mode: ToolExecutionMode) {
        self.tool_execution = mode;
    }
    pub fn set_block_images(&mut self, block_images: bool) {
        self.block_images = block_images;
    }
    pub fn set_tool_result_image_options(
        &mut self,
        options: Option<crate::tools::image::ProcessImageOptions>,
    ) {
        self.tool_result_image_options = options;
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, AgentState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Subscribe to lifecycle events (upstream `Agent.subscribe`).
    pub fn subscribe<F>(&self, listener: F) -> Box<dyn FnOnce() + Send>
    where
        F: Fn(RichAgentEvent, Option<Arc<AtomicBool>>) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((id, Arc::new(listener)));
        let listeners = Arc::downgrade(&self.listeners);
        Box::new(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .retain(|(listener_id, _)| *listener_id != id);
            }
        })
    }

    /// Clear the transcript, runtime state, and both pending queues.
    pub fn reset(&self) -> Result<(), AgentRunError> {
        if self.is_streaming() {
            return Err(AgentRunError::AlreadyProcessingPrompt);
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.messages.clear();
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        state.error_message = None;
        drop(state);
        self.clear_all_queues();
        Ok(())
    }

    pub fn streaming_message(&self) -> Option<AgentMessage> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .streaming_message
            .clone()
    }

    pub fn pending_tool_calls(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_tool_calls
            .clone()
    }

    pub fn error_message(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error_message
            .clone()
    }

    pub fn prompt_text<T: Into<String>>(
        &self,
        text: T,
    ) -> impl Future<Output = Result<(), AgentRunError>> + '_ {
        let message = crate::agent::user_text_prompt(text, pi_ai::types::now_ms());
        self.prompt(message)
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.steering_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.steering_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode = mode;
    }
    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.follow_up_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode = mode;
    }

    /// Queue a message to be injected after the current assistant turn
    /// finishes (upstream `Agent.steer`).
    pub fn steer(&self, message: AgentMessage) {
        self.steering_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop
    /// (upstream `Agent.followUp`).
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enqueue(message);
    }

    pub fn clear_steering_queue(&self) {
        self.steering_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
    pub fn clear_follow_up_queue(&self) {
        self.follow_up_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .has_items()
            || self
                .follow_up_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .has_items()
    }

    /// Abort the current run, if one is active (upstream `Agent.abort`).
    pub fn abort(&self) {
        if let Some(run) = &*self
            .active_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            run.abort.store(true, Ordering::SeqCst);
        }
    }

    /// Return the cancellation flag for the active run, if one exists.
    pub fn signal(&self) -> Option<Arc<AtomicBool>> {
        self.active_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|run| run.abort.clone())
    }

    pub fn is_streaming(&self) -> bool {
        self.active_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    /// Wait until the current run and all of its awaited listeners settle.
    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self
                .active_run
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .map(|run| run.done.clone().notified_owned());
            let Some(notified) = notified else {
                return;
            };
            notified.await;
        }
    }

    /// Start a prompt run (upstream `Agent.prompt`). Returns after settlement
    /// of the run and its awaited listeners.
    pub async fn prompt(&self, message: AgentMessage) -> Result<(), AgentRunError> {
        self.run_prompt_messages(vec![message], false)
            .await
            .map(|_| ())
    }

    /// Run one or more prompts and return the messages appended to the
    /// stateful transcript by this invocation. This is the harness-facing
    /// equivalent of upstream `Agent.prompt` when callers need to persist the
    /// resulting lane entries themselves.
    pub async fn prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<Vec<AgentMessage>, AgentRunError> {
        self.run_prompt_messages(messages, false).await
    }

    /// Run prompts and return both the durable message delta and the rich
    /// lifecycle events produced by this invocation.
    pub async fn prompt_messages_with_events(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(Vec<AgentMessage>, Vec<RichAgentEvent>), AgentRunError> {
        self.run_prompt_messages_with_events(messages, false).await
    }

    /// Continue from the current transcript (upstream `Agent.continue`).
    pub async fn continue_(&self) -> Result<(), AgentRunError> {
        if self.is_streaming() {
            return Err(AgentRunError::AlreadyProcessingContinuation);
        }
        let last = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.messages.last().cloned()
        };
        let Some(last) = last else {
            return Err(AgentRunError::NoMessagesToContinue);
        };
        if last.role() == "assistant" {
            let queued_steering = self
                .steering_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .drain();
            if !queued_steering.is_empty() {
                self.run_prompt_messages(queued_steering, true)
                    .await
                    .map(|_| ())?;
                return Ok(());
            }
            let queued_follow_ups = self
                .follow_up_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .drain();
            if !queued_follow_ups.is_empty() {
                self.run_prompt_messages(queued_follow_ups, false)
                    .await
                    .map(|_| ())?;
                return Ok(());
            }
            return Err(AgentRunError::CannotContinueFromAssistant);
        }
        self.run_continuation().await
    }

    async fn run_prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering: bool,
    ) -> Result<Vec<AgentMessage>, AgentRunError> {
        let (messages, _) = self
            .run_prompt_messages_with_events(messages, skip_initial_steering)
            .await?;
        Ok(messages)
    }

    async fn run_prompt_messages_with_events(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering: bool,
    ) -> Result<(Vec<AgentMessage>, Vec<RichAgentEvent>), AgentRunError> {
        let (signal, lease) = self.begin_run(AgentRunError::AlreadyProcessingPrompt)?;
        let result = self
            .run_prompt_messages_with_events_inner(messages, skip_initial_steering, signal)
            .await;
        drop(lease);
        Ok(result)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    async fn run_prompt_messages_with_events_inner(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering: bool,
        signal: Arc<AtomicBool>,
    ) -> (Vec<AgentMessage>, Vec<RichAgentEvent>) {
        let mut skip = skip_initial_steering;
        let (model, system_prompt, tools) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            (
                state.model.clone(),
                state.system_prompt.clone(),
                state.tools.clone(),
            )
        };
        let mut context = AgentContext::new(Some(system_prompt), tools);
        let prior_messages = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.messages.clone()
        };
        context.messages = prior_messages.clone();
        let config = self.build_config(model, &mut skip, signal);
        let mut events: Vec<RichAgentEvent> = Vec::new();
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let event_dispatcher = tokio::spawn(dispatch_agent_events(
            event_receiver,
            self.listeners.clone(),
            config
                .signal
                .clone()
                .expect("agent runs always have a signal"),
        ));
        let new_messages = run_rich_agent_loop(messages, &mut context, &config, &mut |e| {
            self.apply_event(&e);
            events.push(e.clone());
            let _ = event_sender.send(e);
        })
        .await;
        drop(event_sender);
        event_dispatcher
            .await
            .expect("agent event dispatcher should not panic");
        // The loop's returned delta is the durable event stream, including a
        // failed overflow response. Its mutable context is the active
        // provider-facing state and may intentionally exclude that response
        // after a successful compact-and-retry. Keep the two concepts
        // separate: listeners/session owners receive the durable delta while
        // the stateful Agent continues from the rebuilt active context.
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.messages = context.messages;
        }
        (new_messages, events)
    }

    async fn run_continuation(&self) -> Result<(), AgentRunError> {
        let (signal, lease) = self.begin_run(AgentRunError::AlreadyProcessingContinuation)?;
        let result = self.run_continuation_inner(signal).await;
        drop(lease);
        Ok(result)
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    async fn run_continuation_inner(&self, signal: Arc<AtomicBool>) {
        let (model, system_prompt, tools) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            (
                state.model.clone(),
                state.system_prompt.clone(),
                state.tools.clone(),
            )
        };
        let mut context = AgentContext::new(Some(system_prompt), tools);
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            context.messages = state.messages.clone();
        }
        let config = self.build_config(model, &mut false, signal);
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let event_dispatcher = tokio::spawn(dispatch_agent_events(
            event_receiver,
            self.listeners.clone(),
            config
                .signal
                .clone()
                .expect("agent runs always have a signal"),
        ));
        let _new_messages = run_rich_agent_loop(Vec::new(), &mut context, &config, &mut |e| {
            self.apply_event(&e);
            let _ = event_sender.send(e);
        })
        .await;
        drop(event_sender);
        event_dispatcher
            .await
            .expect("agent event dispatcher should not panic");
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.messages = context.messages;
        }
    }

    fn build_config(
        &self,
        model: pi_ai::model::Model,
        skip_initial_steering: &mut bool,
        signal: Arc<AtomicBool>,
    ) -> RichAgentLoopConfig {
        let mut config =
            RichAgentLoopConfig::new(model, self.stream_fn.clone(), Some(signal.clone()));
        let runtime_options = self
            .runtime_options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        config.stream_fn_with_options = self.stream_fn_with_options.clone();
        config.on_payload = self.on_payload.clone();
        config.on_response = self.on_response.clone();
        config.convert_to_llm = self.convert_to_llm.clone();
        config.block_images = self.block_images;
        config.tool_result_image_options = self.tool_result_image_options;
        config.before_tool_call = self.before_tool_call.clone();
        config.after_tool_call = self.after_tool_call.clone();
        config.should_stop_after_turn = self.should_stop_after_turn.clone();
        config.tool_execution = self.tool_execution;
        config.session_id = self.session_id.clone();
        // AgentState is the public mutable source of truth. A harness or a
        // shared Agent handle can update it while this Agent is held in an
        // Arc, so a separate field cannot reliably carry live changes into
        // the next provider request.
        config.reasoning = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .thinking_level;
        config.stream_options = runtime_options.stream_options;
        config.retry_policy = runtime_options.retry_policy;
        config.max_retry_delay_ms = runtime_options.max_retry_delay_ms;
        config.get_api_key = runtime_options.get_api_key;
        config.api_key = runtime_options.api_key;
        config.prepare_next_turn = self.prepare_next_turn.clone();
        config.prepare_next_turn_with_context = self.prepare_next_turn_with_context.clone();
        config.overflow_recovery = self.overflow_recovery.clone();
        let steer = Arc::clone(&self.steering_queue);
        let initial_skip = *skip_initial_steering;
        let poll_count = Arc::new(AtomicUsize::new(0));
        let steering_hook: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
            let steer = Arc::clone(&steer);
            let poll_count = poll_count.clone();
            async move {
                // Upstream skips only the initial steering poll in
                // `runPromptMessages({ skipInitialSteeringPoll: true })`.
                if initial_skip && poll_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Vec::new();
                }
                steer
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .drain()
            }
            .boxed()
        });
        let follow = Arc::clone(&self.follow_up_queue);
        let follow_up_hook: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
            let follow = Arc::clone(&follow);
            async move {
                follow
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .drain()
            }
            .boxed()
        });
        config.get_steering_messages = steering_hook;
        config.get_follow_up_messages = follow_up_hook;
        config.retry_signal = Some(signal);
        config
    }

    fn apply_event(&self, event: &RichAgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match event {
            RichAgentEvent::MessageStart { message }
            | RichAgentEvent::MessageUpdate { message, .. } => {
                state.is_streaming = true;
                state.streaming_message = Some(message.clone());
            }
            RichAgentEvent::MessageEnd { message } => {
                state.streaming_message = None;
                state.messages.push(message.clone());
            }
            RichAgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                if !state.pending_tool_calls.iter().any(|id| id == tool_call_id) {
                    state.pending_tool_calls.push(tool_call_id.clone());
                }
            }
            RichAgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                state.pending_tool_calls.retain(|id| id != tool_call_id);
            }
            RichAgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Core(Message::Assistant(assistant)) = message {
                    if assistant.stop_reason() == Some(StopReason::Error) {
                        state.error_message = assistant.error_message().map(str::to_string);
                    }
                }
            }
            RichAgentEvent::AgentEnd { .. } => {
                state.streaming_message = None;
            }
            RichAgentEvent::AgentStart
            | RichAgentEvent::AutoRetryStart { .. }
            | RichAgentEvent::AutoRetryEnd { .. }
            | RichAgentEvent::TurnStart
            | RichAgentEvent::ToolExecutionUpdate { .. } => {}
        }
    }

    fn begin_run(
        &self,
        already_running: AgentRunError,
    ) -> Result<(Arc<AtomicBool>, RunLease), AgentRunError> {
        let mut active = self
            .active_run
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if active.is_some() {
            return Err(already_running);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let done = Arc::new(tokio::sync::Notify::new());
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.is_streaming = true;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
            state.error_message = None;
        }
        *active = Some(ActiveRun {
            abort: abort.clone(),
            done: done.clone(),
        });
        Ok((
            abort,
            RunLease {
                active_run: self.active_run.clone(),
                done,
                state: self.state.clone(),
            },
        ))
    }
}

/// Dispatch events in emission order while the agent loop is still active.
///
/// The sender is closed by the owning run only after `AgentEnd` has been
/// emitted. The run awaits this task before dropping its lease, so
/// `is_streaming()` remains true until the final subscriber settles.
async fn dispatch_agent_events(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<RichAgentEvent>,
    listeners: Arc<Mutex<Vec<(usize, AgentListener)>>>,
    signal: Arc<AtomicBool>,
) {
    while let Some(event) = receiver.recv().await {
        let current_listeners = listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for (_, listener) in &current_listeners {
            let _ = std::panic::AssertUnwindSafe(listener(event.clone(), Some(signal.clone())))
                .catch_unwind()
                .await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_ai::providers::{
        faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
        RegisterFauxProviderOptions,
    };
    use pi_ai::types::ContentBlock;
    use std::time::Duration;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn scripted_stream(core: FauxProviderCore) -> StreamFn {
        Arc::new(move |model: &pi_ai::model::Model, ctx: &Context| core.stream(model, ctx, None))
    }

    fn steer_msg(text: &str) -> AgentMessage {
        crate::agent::user_text_prompt(text, 1)
    }

    #[test]
    fn rich_loop_forwards_options_and_runs_payload_response_hooks() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("configured")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let signal = Arc::new(AtomicBool::new(false));
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_options = Arc::new(Mutex::new(
                Vec::<(Option<String>, Option<ThinkingLevel>, Option<String>, bool)>::new(),
            ));

            let payload_seen = seen.clone();
            let on_payload: pi_ai::types::OnPayloadFn = Arc::new(move |payload, _model| {
                let payload_seen = payload_seen.clone();
                Box::pin(async move {
                    payload_seen
                        .lock().unwrap_or_else(|error| error.into_inner())
                        .push(format!("payload:{}", payload["turn"]));
                    Some(serde_json::json!({"replaced": true}))
                })
            });
            let response_seen = seen.clone();
            let on_response: pi_ai::model::OnResponseFn = Arc::new(move |response, _model| {
                response_seen
                    .lock().unwrap_or_else(|error| error.into_inner())
                    .push(format!("response:{}", response.status));
            });
            let seen_options_for_stream = seen_options.clone();
            let core_for_stream = core.clone();
            let stream_fn: StreamFnWithOptions = Arc::new(move |model, context, options| {
                seen_options_for_stream.lock().unwrap_or_else(|error| error.into_inner()).push((
                    options.base.session_id.clone(),
                    options.reasoning,
                    options.base.base.api_key.clone(),
                    options.base.abort_signal.is_some(),
                ));
                let _ = options
                    .base
                    .on_payload
                    .as_ref()
                    .and_then(|hook| hook(serde_json::json!({"turn": 1}), model.clone()).now_or_never());
                core_for_stream.stream(model, context, Some(options))
            });

            let mut config = RichAgentLoopConfig::new(
                model,
                Arc::new(|_, _| pi_ai::create_error_stream("test", "test", "test", "unused".into())),
                Some(signal.clone()),
            );
            config.stream_fn_with_options = Some(stream_fn);
            config.reasoning = Some(ThinkingLevel::High);
            config.session_id = Some("session-rich".into());
            config.on_payload = Some(on_payload);
            config.on_response = Some(on_response);
            config.api_key = Some("key-rich".into());

            let mut context = AgentContext::new(Some("system".into()), Vec::new());
            let messages = run_rich_agent_loop(
                vec![steer_msg("hello")],
                &mut context,
                &config,
                &mut |_| {},
            )
            .await;

            assert!(messages.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant
                        .content()
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "configured"))
            )));
            assert_eq!(
                *seen_options.lock().unwrap_or_else(|error| error.into_inner()),
                vec![(
                    Some("session-rich".into()),
                    Some(ThinkingLevel::High),
                    Some("key-rich".into()),
                    true,
                )]
            );
            assert_eq!(
                *seen.lock().unwrap_or_else(|error| error.into_inner()),
                vec!["payload:1".to_string(), "response:200".to_string()]
            );
            assert!(!signal.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn rich_loop_prepare_hooks_run_after_turn_end_and_update_next_turn_options() {
        let rt = rt();
        rt.block_on(async {
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
            let model = core.get_model(None).unwrap().clone();
            let calls = Arc::new(AtomicUsize::new(0));
            let options_seen = Arc::new(Mutex::new(
                Vec::<(Option<String>, Option<ThinkingLevel>)>::new(),
            ));
            let stream_fn = {
                let core = core.clone();
                let options_seen = options_seen.clone();
                Arc::new(
                    move |model: &pi_ai::model::Model,
                          context: &Context,
                          options: &pi_ai::types::SimpleStreamOptions| {
                        options_seen
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push((options.base.session_id.clone(), options.reasoning));
                        core.stream(model, context, Some(options))
                    },
                ) as StreamFnWithOptions
            };
            let order = Arc::new(Mutex::new(Vec::<String>::new()));
            let order_for_prepare = order.clone();
            let calls_for_prepare = calls.clone();
            let mut config = RichAgentLoopConfig::new(
                model,
                Arc::new(|_, _| {
                    pi_ai::create_error_stream("test", "test", "test", "unused".into())
                }),
                None,
            );
            config.stream_fn_with_options = Some(stream_fn);
            config.session_id = Some("turn-1".into());
            config.prepare_next_turn_with_context = Some(Arc::new(move |turn, _| {
                order_for_prepare
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push("prepare".into());
                assert!(turn
                    .context
                    .messages
                    .iter()
                    .any(|message| matches!(message, AgentMessage::Core(Message::Assistant(_)))));
                if calls_for_prepare.fetch_add(1, Ordering::SeqCst) == 0 {
                    Box::pin(async {
                        Some(RichAgentLoopTurnUpdate {
                            stream_options: Some(pi_ai::types::SimpleStreamOptions {
                                base: pi_ai::types::StreamOptions {
                                    session_id: Some("turn-2".into()),
                                    ..Default::default()
                                },
                                reasoning: Some(ThinkingLevel::Low),
                                ..Default::default()
                            }),
                            ..Default::default()
                        })
                    })
                } else {
                    Box::pin(async { None })
                }
            }));
            let order_for_emit = order.clone();
            let mut context = AgentContext::new(None, Vec::new());
            let follow_up_calls = Arc::new(AtomicUsize::new(0));
            let follow_up_calls_for_hook = follow_up_calls.clone();
            config.get_follow_up_messages = Arc::new(move |()| {
                let count = follow_up_calls_for_hook.fetch_add(1, Ordering::SeqCst);
                async move {
                    if count == 0 {
                        vec![steer_msg("repeat")]
                    } else {
                        Vec::new()
                    }
                }
                .boxed()
            });
            let _ = run_rich_agent_loop(
                vec![steer_msg("start")],
                &mut context,
                &config,
                &mut |event| {
                    if matches!(event, RichAgentEvent::TurnEnd { .. }) {
                        order_for_emit
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push("turn_end".into());
                    }
                },
            )
            .await;

            assert_eq!(
                *options_seen
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
                vec![
                    (Some("turn-1".into()), None),
                    (Some("turn-2".into()), Some(ThinkingLevel::Low)),
                ]
            );
            assert_eq!(
                *order.lock().unwrap_or_else(|error| error.into_inner()),
                vec!["turn_end", "prepare", "turn_end", "prepare"]
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn rich_loop_api_key_resolver_precedes_fallback_and_panics_are_isolated() {
        let rt = rt();
        rt.block_on(async {
            async fn run_with_resolver(
                resolver: ApiKeyResolver,
                expected: &str,
            ) -> Vec<AgentMessage> {
                let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
                core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("safe")],
                    FauxAssistantOptions::default(),
                ))]);
                let model = core.get_model(None).unwrap().clone();
                let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
                let seen_for_stream = seen.clone();
                let core_for_stream = core.clone();
                let stream_fn: StreamFnWithOptions = Arc::new(move |model, context, options| {
                    seen_for_stream
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(options.base.base.api_key.clone());
                    core_for_stream.stream(model, context, Some(options))
                });
                let mut config = RichAgentLoopConfig::new(
                    model,
                    Arc::new(|_, _| {
                        pi_ai::create_error_stream("test", "test", "test", "unused".into())
                    }),
                    None,
                );
                config.stream_fn_with_options = Some(stream_fn);
                config.get_api_key = Some(resolver);
                config.api_key = Some("fallback-key".into());
                let mut context = AgentContext::new(None, Vec::new());
                let messages = run_rich_agent_loop(
                    vec![steer_msg("resolve key")],
                    &mut context,
                    &config,
                    &mut |_| {},
                )
                .await;
                assert_eq!(
                    *seen.lock().unwrap_or_else(|error| error.into_inner()),
                    vec![Some(expected.to_string())]
                );
                messages
            }

            let resolved = run_with_resolver(
                Arc::new(|provider| {
                    assert_eq!(provider, "faux");
                    Some("resolved-key".into())
                }),
                "resolved-key",
            )
            .await;
            assert!(resolved.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(StopReason::Stop)
            )));

            let recovered =
                run_with_resolver(Arc::new(|_| panic!("resolver panic")), "fallback-key").await;
            assert!(recovered.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(StopReason::Stop)
            )));
        });
    }

    #[test]
    fn rich_loop_prepare_next_turn_without_context_is_supported_and_panics_are_isolated() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("safe")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let called = Arc::new(AtomicUsize::new(0));
            let called_for_hook = called.clone();
            let core_for_stream = core.clone();
            let stream_fn: StreamFnWithOptions = Arc::new(move |model, context, options| {
                let _ = options
                    .base
                    .on_payload
                    .as_ref()
                    .and_then(|hook| hook(serde_json::json!({}), model.clone()).now_or_never());
                core_for_stream.stream(model, context, Some(options))
            });
            let mut config = RichAgentLoopConfig::new(
                model,
                Arc::new(|_, _| {
                    pi_ai::create_error_stream("test", "test", "test", "unused".into())
                }),
                None,
            );
            config.stream_fn_with_options = Some(stream_fn);
            config.prepare_next_turn = Some(Arc::new(move |_| {
                called_for_hook.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { panic!("hook panic") })
            }));
            config.on_payload = Some(Arc::new(|_, _| Box::pin(async { panic!("payload panic") })));
            config.on_response = Some(Arc::new(|_, _| panic!("response panic")));
            let mut context = AgentContext::new(None, Vec::new());
            let messages =
                run_rich_agent_loop(vec![steer_msg("safe")], &mut context, &config, &mut |_| {})
                    .await;
            assert_eq!(called.load(Ordering::SeqCst), 1);
            assert!(messages.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(StopReason::Stop)
            )));
        });
    }

    #[test]
    fn rich_loop_abort_interrupts_a_pending_prepare_hook() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("finished turn")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let signal = Arc::new(AtomicBool::new(false));
            let mut config =
                RichAgentLoopConfig::new(model, scripted_stream(core), Some(signal.clone()));
            config.prepare_next_turn = Some(Arc::new(move |signal| {
                signal.unwrap().store(true, Ordering::SeqCst);
                Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    None
                })
            }));
            let mut context = AgentContext::new(None, Vec::new());
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                run_rich_agent_loop(
                    vec![steer_msg("interrupt prepare")],
                    &mut context,
                    &config,
                    &mut |_| {},
                ),
            )
            .await
            .expect("abort should interrupt the pending preparation hook");
            assert!(signal.load(Ordering::SeqCst));
            assert!(result.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(StopReason::Stop)
            )));
        });
    }

    fn terminating_tool(name: &str, terminate: bool) -> AgentTool {
        let name = name.to_string();
        let tool_name = name.clone();
        AgentTool::new(
            pi_ai::types::json_tool(
                &name,
                "test terminating tool",
                &serde_json::json!({"type": "object", "properties": {}}),
            ),
            name,
            Arc::new(move |_, _, _, _| {
                let tool_name = tool_name.clone();
                Box::pin(async move {
                    Ok(crate::tools::AgentToolResult {
                        terminate,
                        ..crate::tools::AgentToolResult::text(format!("{tool_name} done"))
                    })
                })
            }),
        )
    }

    #[test]
    fn pending_queue_drain_modes() {
        let mut all = PendingMessageQueue::new(QueueMode::All);
        all.enqueue(steer_msg("a"));
        all.enqueue(steer_msg("b"));
        assert_eq!(all.drain().len(), 2);
        assert!(!all.has_items());

        let mut one = PendingMessageQueue::new(QueueMode::OneAtATime);
        one.enqueue(steer_msg("a"));
        one.enqueue(steer_msg("b"));
        assert_eq!(one.drain().len(), 1);
        assert!(one.has_items());
        one.clear();
        assert!(!one.has_items());
    }

    #[test]
    fn rich_loop_executes_tool_batch_and_emits_execution_events() {
        let rt = rt();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("pi-agent-rich-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let cwd = dir.to_string_lossy().to_string();
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::tool_call("tool-1", "bash", serde_json::json!({"command": "echo from-rich"}))],
                    FauxAssistantOptions { stop_reason: Some(pi_ai::types::StopReason::ToolUse), ..Default::default() },
                )),
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("done")],
                    FauxAssistantOptions::default(),
                )),
            ]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let tools: Vec<AgentTool> = vec![crate::tools::bash_tool(cwd)];
            let mut context = AgentContext::new(Some("test".into()), tools);
            let cfg = RichAgentLoopConfig::new(model, stream_fn, None);
            let mut events: Vec<RichAgentEvent> = Vec::new();
            let prompts = vec![steer_msg("hello")];
            let new_messages = run_rich_agent_loop(prompts, &mut context, &cfg, &mut |e| events.push(e)).await;

            assert!(events.iter().any(|e| matches!(e, RichAgentEvent::ToolExecutionStart { tool_name, .. } if tool_name == "bash")));
            assert!(events.iter().any(|e| matches!(e, RichAgentEvent::ToolExecutionEnd { is_error: false, .. })));
            assert!(events.iter().any(|e| {
                matches!(
                    e,
                    RichAgentEvent::ToolExecutionUpdate {
                        tool_name,
                        partial_result,
                        ..
                    } if tool_name == "bash" && partial_result["content"].is_array()
                )
            }));
            assert!(events.iter().any(|e| matches!(e, RichAgentEvent::AgentEnd { .. })));
            let has_tool_result = new_messages.iter().any(|m| {
                matches!(m, AgentMessage::Core(Message::ToolResult(r)) if !r.is_error())
            });
            assert!(has_tool_result);
        });
    }

    #[test]
    fn terminate_hints_require_every_parallel_tool_to_opt_in() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            let message = faux_assistant_message(
                vec![
                    ContentBlock::tool_call("terminate-1", "stop", serde_json::json!({})),
                    ContentBlock::tool_call("terminate-2", "continue", serde_json::json!({})),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let context = AgentContext::new(
                Some("test".into()),
                vec![
                    terminating_tool("stop", true),
                    terminating_tool("continue", false),
                ],
            );
            let mut events = Vec::new();
            let batch =
                execute_tool_batch(&message, &context, &cfg, &mut |event| events.push(event)).await;
            assert_eq!(batch.messages.len(), 2);
            assert!(!batch.terminate);
            let event_terminations: std::collections::HashMap<_, _> = events
                .iter()
                .filter_map(|event| match event {
                    RichAgentEvent::ToolExecutionEnd {
                        tool_call_id,
                        result,
                        ..
                    } => Some((tool_call_id.as_str(), result.terminate)),
                    _ => None,
                })
                .collect();
            assert_eq!(event_terminations.get("terminate-1"), Some(&true));
            assert_eq!(event_terminations.get("terminate-2"), Some(&false));

            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            let context = AgentContext::new(
                Some("test".into()),
                vec![
                    terminating_tool("stop", true),
                    terminating_tool("finish", true),
                ],
            );
            let message = faux_assistant_message(
                vec![
                    ContentBlock::tool_call("terminate-3", "stop", serde_json::json!({})),
                    ContentBlock::tool_call("terminate-4", "finish", serde_json::json!({})),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let mut terminating_events = Vec::new();
            let batch = execute_tool_batch(&message, &context, &cfg, &mut |event| {
                terminating_events.push(event)
            })
            .await;
            assert!(batch.terminate);
            assert_eq!(
                terminating_events
                    .iter()
                    .filter_map(|event| match event {
                        RichAgentEvent::ToolExecutionEnd { result, .. } => Some(result.terminate),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec![true, true]
            );
        });
    }

    fn value_tool(name: &str) -> AgentTool {
        let name = name.to_string();
        AgentTool::new(
            pi_ai::types::json_tool(
                &name,
                "test value tool",
                &serde_json::json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }),
            ),
            name.clone(),
            Arc::new(move |_, args, _, _| {
                let value = args
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Box::pin(async move {
                    Ok(crate::tools::AgentToolResult {
                        content: vec![ContentBlock::text(format!("value:{value}"))],
                        details: Some(serde_json::json!({"value": value})),
                        ..Default::default()
                    })
                })
            }),
        )
    }

    #[test]
    fn parallel_tool_events_finish_in_completion_order_and_results_stay_in_source_order() {
        let rt = rt();
        rt.block_on(async {
            let name = "delayed".to_string();
            let tool = AgentTool::new(
                pi_ai::types::json_tool(
                    &name,
                    "test delayed tool",
                    &serde_json::json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }),
                ),
                name.clone(),
                Arc::new(move |_, args, _, on_update| {
                    let value = args["value"].as_str().unwrap().to_string();
                    Box::pin(async move {
                        let delay = if value == "slow" { 40 } else { 5 };
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        if let Some(on_update) = on_update {
                            on_update(&crate::tools::AgentToolResult::text(format!(
                                "update:{value}"
                            )));
                        }
                        Ok(crate::tools::AgentToolResult {
                            content: vec![ContentBlock::text(format!("done:{value}"))],
                            details: Some(serde_json::json!({"value": value})),
                            ..Default::default()
                        })
                    })
                }),
            );
            let context = AgentContext::new(Some("test".into()), vec![tool]);
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            let message = faux_assistant_message(
                vec![
                    ContentBlock::tool_call(
                        "call-slow",
                        "delayed",
                        serde_json::json!({"value": "slow"}),
                    ),
                    ContentBlock::tool_call(
                        "call-fast",
                        "delayed",
                        serde_json::json!({"value": "fast"}),
                    ),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let mut events = Vec::new();
            let batch =
                execute_tool_batch(&message, &context, &cfg, &mut |event| events.push(event)).await;

            let end_ids: Vec<String> = events
                .iter()
                .filter_map(|event| match event {
                    RichAgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        Some(tool_call_id.clone())
                    }
                    _ => None,
                })
                .collect();
            let result_ids: Vec<String> = events
                .iter()
                .filter_map(|event| match event {
                    RichAgentEvent::MessageEnd {
                        message: AgentMessage::Core(Message::ToolResult(result)),
                    } => Some(result.tool_call_id().to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(end_ids, vec!["call-fast", "call-slow"]);
            assert_eq!(result_ids, vec!["call-slow", "call-fast"]);
            assert_eq!(batch.messages.len(), 2);
            assert!(events.iter().any(|event| {
                matches!(
                    event,
                    RichAgentEvent::ToolExecutionUpdate { partial_result, .. }
                        if partial_result["content"].as_array().is_some()
                )
            }));
        });
    }

    #[test]
    fn parallel_preparation_errors_are_emitted_and_after_hook_overrides_results() {
        let rt = rt();
        rt.block_on(async {
            let tool = value_tool("echo");
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let mut cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            let mut bmp = vec![0u8; 58];
            bmp[0..2].copy_from_slice(b"BM");
            bmp[2..6].copy_from_slice(&58u32.to_le_bytes());
            bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
            bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
            bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
            bmp[22..26].copy_from_slice(&1i32.to_le_bytes());
            bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
            bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
            bmp[34..38].copy_from_slice(&4u32.to_le_bytes());
            bmp[56] = 0xff;
            let bmp = crate::tools::image::encode_base64(&bmp);
            cfg.after_tool_call = Some(Arc::new(move |context, _| {
                let bmp = bmp.clone();
                Box::pin(async move {
                    assert_eq!(context.tool_name, "echo");
                    Some(AfterToolCallResult {
                        content: Some(vec![
                            ContentBlock::text("overridden"),
                            ContentBlock::Image {
                                data: bmp,
                                mime_type: "image/bmp".to_string(),
                            },
                        ]),
                        terminate: Some(true),
                        ..Default::default()
                    })
                })
            }));
            cfg.tool_result_image_options = Some(crate::tools::image::ProcessImageOptions {
                auto_resize_images: false,
                ..Default::default()
            });
            let context = AgentContext::new(Some("test".into()), vec![tool]);
            let message = faux_assistant_message(
                vec![
                    ContentBlock::tool_call("invalid", "echo", serde_json::json!({})),
                    ContentBlock::tool_call("valid", "echo", serde_json::json!({"value": "ok"})),
                ],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let mut events = Vec::new();
            let batch =
                execute_tool_batch(&message, &context, &cfg, &mut |event| events.push(event)).await;

            let ends: Vec<(String, bool)> = events
                .iter()
                .filter_map(|event| match event {
                    RichAgentEvent::ToolExecutionEnd {
                        tool_call_id,
                        is_error,
                        ..
                    } => Some((tool_call_id.clone(), *is_error)),
                    _ => None,
                })
                .collect();
            assert_eq!(ends.len(), 2);
            assert!(ends
                .iter()
                .any(|(id, is_error)| id == "invalid" && *is_error));
            assert!(ends
                .iter()
                .any(|(id, is_error)| id == "valid" && !*is_error));
            assert!(!batch.terminate);
            assert_eq!(batch.messages[0].tool_call_id(), "invalid");
            assert_eq!(batch.messages[1].tool_call_id(), "valid");
            assert!(matches!(
                &batch.messages[1],
                ToolResultMessage::ToolResult { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text, .. } if text == "overridden"
                    ))
                    && content.iter().any(|block| matches!(
                        block,
                        ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
                    ))
                    && content.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text, .. }
                            if text == "[Image converted from image/bmp to image/png.]"
                    ))
            ));
        });
    }

    #[test]
    fn before_tool_call_can_mutate_validated_args_without_revalidation() {
        let rt = rt();
        rt.block_on(async {
            let observed = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
            let observed_by_tool = observed.clone();
            let tool = AgentTool::new(
                pi_ai::types::json_tool(
                    "echo",
                    "test mutable args tool",
                    &serde_json::json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }),
                ),
                "echo",
                Arc::new(move |_, args, _, _| {
                    observed_by_tool
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(args["value"].clone());
                    Box::pin(async { Ok(crate::tools::AgentToolResult::text("ok")) })
                }),
            );
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let mut cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            cfg.before_tool_call = Some(Arc::new(|context, _| {
                context.args["value"] = serde_json::json!(123);
                Box::pin(async { None })
            }));
            let context = AgentContext::new(Some("test".into()), vec![tool]);
            let message = faux_assistant_message(
                vec![ContentBlock::tool_call(
                    "mutable",
                    "echo",
                    serde_json::json!({"value": "hello"}),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let _ = execute_tool_batch(&message, &context, &cfg, &mut |_| {}).await;
            assert_eq!(
                *observed.lock().unwrap_or_else(|error| error.into_inner()),
                vec![serde_json::json!(123)]
            );
        });
    }

    #[test]
    fn blocked_tool_terminate_hint_stops_the_batch_without_execution() {
        let rt = rt();
        rt.block_on(async {
            let executions = Arc::new(AtomicUsize::new(0));
            let executions_by_tool = executions.clone();
            let tool = AgentTool::new(
                pi_ai::types::json_tool(
                    "blocked",
                    "must be blocked before execution",
                    &serde_json::json!({"type": "object", "properties": {}}),
                ),
                "blocked",
                Arc::new(move |_, _, _, _| {
                    executions_by_tool.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(crate::tools::AgentToolResult::text("unexpected")) })
                }),
            );
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let mut cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            cfg.before_tool_call = Some(Arc::new(|_, _| {
                Box::pin(async {
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("policy denied".to_string()),
                        terminate: true,
                    })
                })
            }));
            let context = AgentContext::new(Some("test".into()), vec![tool]);
            let message = faux_assistant_message(
                vec![ContentBlock::tool_call(
                    "blocked-call",
                    "blocked",
                    serde_json::json!({}),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let mut events = Vec::new();
            let batch =
                execute_tool_batch(&message, &context, &cfg, &mut |event| events.push(event)).await;

            assert!(batch.terminate);
            assert_eq!(executions.load(Ordering::SeqCst), 0);
            assert_eq!(batch.messages.len(), 1);
            assert!(batch.messages[0].is_error());
            assert!(matches!(
                &batch.messages[0],
                ToolResultMessage::ToolResult { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text, .. } if text == "policy denied"
                    ))
            ));
            assert!(events.iter().any(|event| matches!(
                event,
                RichAgentEvent::ToolExecutionEnd { result, is_error: true, .. }
                    if result.terminate
            )));
        });
    }

    #[test]
    fn late_tool_updates_after_settlement_are_ignored() {
        let rt = rt();
        rt.block_on(async {
            let tool = AgentTool::new(
                pi_ai::types::json_tool(
                    "late",
                    "test late update tool",
                    &serde_json::json!({"type": "object", "properties": {}}),
                ),
                "late",
                Arc::new(move |_, _, _, on_update| {
                    Box::pin(async move {
                        if let Some(on_update) = on_update {
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                on_update(&crate::tools::AgentToolResult::text("late"));
                            });
                        }
                        Ok(crate::tools::AgentToolResult::text("done"))
                    })
                }),
            );
            let context = AgentContext::new(Some("test".into()), vec![tool]);
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let cfg = RichAgentLoopConfig::new(model, scripted_stream(core), None);
            let message = faux_assistant_message(
                vec![ContentBlock::tool_call(
                    "late-call",
                    "late",
                    serde_json::json!({}),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let mut events = Vec::new();
            let _ =
                execute_tool_batch(&message, &context, &cfg, &mut |event| events.push(event)).await;
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            assert!(!events.iter().any(|event| {
                matches!(
                    event,
                    RichAgentEvent::ToolExecutionUpdate { partial_result, .. }
                        if partial_result["content"]
                            .as_array()
                            .and_then(|content| content.first())
                            .and_then(|content| content.get("text"))
                            == Some(&serde_json::Value::String("late".to_string()))
                )
            }));
        });
    }

    #[test]
    fn rich_loop_abort_cancels_inflight_bash_tool() {
        let rt = rt();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!(
                "pi-agent-rich-abort-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let cwd = dir.to_string_lossy().to_string();
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::tool_call(
                    "tool-abort",
                    "bash",
                    serde_json::json!({"command": "sleep 10"}),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let abort = Arc::new(AtomicBool::new(false));
            let config = RichAgentLoopConfig::new(model, stream_fn, Some(abort.clone()));
            let mut context = AgentContext::new(
                Some("test".into()),
                vec![crate::tools::bash_tool(cwd.clone())],
            );
            let task = tokio::spawn(async move {
                run_rich_agent_loop(
                    vec![steer_msg("abort tool")],
                    &mut context,
                    &config,
                    &mut |_| {},
                )
                .await
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            abort.store(true, Ordering::SeqCst);
            let messages = tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("agent abort should stop the bash tool")
                .unwrap();
            assert!(messages.iter().any(|message| {
                matches!(
                    message,
                    AgentMessage::Core(Message::ToolResult(result)) if result.is_error()
                )
            }));
            assert!(messages.iter().any(|message| {
                matches!(
                    message,
                    AgentMessage::Core(Message::Assistant(assistant))
                        if assistant.stop_reason() == Some(StopReason::Aborted)
                )
            }));
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn abort_cancels_a_tool_that_does_not_cooperate_with_its_signal() {
        let rt = rt();
        rt.block_on(async {
            let tool = AgentTool::new(
                pi_ai::types::json_tool(
                    "blocked",
                    "test non-cooperative tool",
                    &serde_json::json!({"type": "object", "properties": {}}),
                ),
                "blocked",
                Arc::new(move |_, _, _, _| {
                    Box::pin(async {
                        std::future::pending::<Result<crate::tools::AgentToolResult, String>>()
                            .await
                    })
                }),
            );
            let context = AgentContext::new(Some("test".into()), vec![tool]);
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let signal = Arc::new(AtomicBool::new(false));
            let config =
                RichAgentLoopConfig::new(model, scripted_stream(core), Some(signal.clone()));
            let message = faux_assistant_message(
                vec![ContentBlock::tool_call(
                    "blocked-call",
                    "blocked",
                    serde_json::json!({}),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            );
            let task = tokio::spawn(async move {
                let mut events = Vec::new();
                let batch = execute_tool_batch(&message, &context, &config, &mut |event| {
                    events.push(event)
                })
                .await;
                (batch, events)
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal.store(true, Ordering::SeqCst);
            let (batch, events) = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .expect("abort should interrupt a non-cooperative tool")
                .expect("tool batch task should not panic");
            assert_eq!(batch.messages.len(), 1);
            assert!(matches!(
                &batch.messages[0],
                ToolResultMessage::ToolResult { is_error, content, .. }
                    if *is_error
                        && content.iter().any(|block| matches!(
                            block,
                            ContentBlock::Text { text, .. } if text == "Operation aborted"
                        ))
            ));
            assert!(events.iter().any(|event| matches!(
                event,
                RichAgentEvent::ToolExecutionEnd { is_error: true, .. }
            )));
        });
    }

    #[test]
    fn rich_loop_truncated_tool_calls_fail_with_error_results() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::tool_call("tool-9", "bash", serde_json::json!({"command": "echo x"}))],
                FauxAssistantOptions { stop_reason: Some(pi_ai::types::StopReason::Length), ..Default::default() },
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let mut context = AgentContext::new(Some("test".into()), Vec::new());
            let cfg = RichAgentLoopConfig::new(model, stream_fn, None);
            let new_messages = run_rich_agent_loop(
                vec![steer_msg("hello")],
                &mut context,
                &cfg,
                &mut |_| {},
            )
            .await;
            let has_error = new_messages.iter().any(|m| {
                matches!(m, AgentMessage::Core(Message::ToolResult(r)) if r.is_error() && r.tool_name() == "bash")
            });
            assert!(has_error, "expected an error tool result for the truncated call");
        });
    }

    #[test]
    fn rich_loop_retry_preserves_attempt_events_and_final_message() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![
                FauxResponseStep::Message(faux_assistant_message(
                    Vec::new(),
                    FauxAssistantOptions {
                        stop_reason: Some(pi_ai::types::StopReason::Error),
                        error_message: Some("overloaded_error".to_string()),
                    },
                )),
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("recovered")],
                    FauxAssistantOptions::default(),
                )),
            ]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let mut context = AgentContext::new(Some("test".into()), Vec::new());
            let mut config = RichAgentLoopConfig::new(model, stream_fn, None);
            config.retry_policy = Some(pi_ai::utils::retry::RetryPolicy {
                enabled: true,
                max_retries: 2,
                base_delay_ms: 0,
            });
            config.retry_signal = Some(Arc::new(AtomicBool::new(false)));
            let mut events = Vec::new();
            let new_messages = run_rich_agent_loop(
                vec![steer_msg("retry me")],
                &mut context,
                &config,
                &mut |event| events.push(event),
            )
            .await;

            let assistant_ends: Vec<&AssistantMessage> = events
                .iter()
                .filter_map(|event| match event {
                    RichAgentEvent::MessageEnd {
                        message: AgentMessage::Core(Message::Assistant(message)),
                    } => Some(message),
                    _ => None,
                })
                .collect();
            assert_eq!(assistant_ends.len(), 2);
            assert_eq!(assistant_ends[0].stop_reason(), Some(StopReason::Error));
            assert_eq!(assistant_ends[1].stop_reason(), Some(StopReason::Stop));
            assert!(events.iter().any(|event| {
                matches!(
                    event,
                    RichAgentEvent::AutoRetryStart {
                        attempt: 1,
                        max_attempts: 2,
                        error_message,
                        ..
                    } if error_message == "overloaded_error"
                )
            }));
            assert!(events.iter().any(|event| {
                matches!(
                    event,
                    RichAgentEvent::AutoRetryEnd {
                        success: true,
                        attempt: 1,
                        final_error: None,
                    }
                )
            }));
            assert_eq!(
                new_messages
                    .iter()
                    .filter(|message| matches!(message, AgentMessage::Core(Message::Assistant(_))))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn rich_loop_abort_retry_keeps_the_failed_attempt_as_final() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                Vec::new(),
                FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::Error),
                    error_message: Some("overloaded_error".to_string()),
                },
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let retry_signal = Arc::new(AtomicBool::new(true));
            let mut config = RichAgentLoopConfig::new(model, stream_fn, None);
            config.retry_policy = Some(pi_ai::utils::retry::RetryPolicy {
                enabled: true,
                max_retries: 2,
                base_delay_ms: 10_000,
            });
            config.retry_signal = Some(retry_signal);
            let mut context = AgentContext::new(Some("test".into()), Vec::new());
            let mut events = Vec::new();
            let new_messages = run_rich_agent_loop(
                vec![steer_msg("abort retry")],
                &mut context,
                &config,
                &mut |event| events.push(event),
            )
            .await;

            assert!(events.iter().any(|event| {
                matches!(
                    event,
                    RichAgentEvent::AutoRetryEnd {
                        success: false,
                        attempt: 1,
                        final_error: Some(error),
                    } if error == "Retry cancelled"
                )
            }));
            assert!(new_messages.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(StopReason::Error)
            )));
            assert!(!new_messages.iter().any(|message| matches!(
                message,
                AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(StopReason::Aborted)
            )));
        });
    }

    #[test]
    fn rich_loop_drains_steering_and_follow_up_messages() {
        let rt = rt();
        rt.block_on(async {
            let poll = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let steering_poll = poll.clone();
            let steering: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
                let poll = steering_poll.clone();
                async move {
                    if poll.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        vec![steer_msg("steered")]
                    } else {
                        Vec::new()
                    }
                }
                .boxed()
            });
            let follow_poll = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let follow_up: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
                let poll = follow_poll.clone();
                async move {
                    if poll.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        vec![steer_msg("followed")]
                    } else {
                        Vec::new()
                    }
                }
                .boxed()
            });

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
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let mut context = AgentContext::new(Some("test".into()), Vec::new());
            let mut cfg = RichAgentLoopConfig::new(model, stream_fn, None);
            cfg.get_steering_messages = steering;
            cfg.get_follow_up_messages = follow_up;
            let mut events: Vec<RichAgentEvent> = Vec::new();
            let new_messages =
                run_rich_agent_loop(vec![steer_msg("hello")], &mut context, &cfg, &mut |e| {
                    events.push(e)
                })
                .await;
            let assistant_count = new_messages
                .iter()
                .filter(|m| matches!(m, AgentMessage::Core(Message::Assistant(_))))
                .count();
            assert_eq!(assistant_count, 2);
            let user_count = new_messages
                .iter()
                .filter(|m| matches!(m, AgentMessage::Core(Message::User(_))))
                .count();
            assert_eq!(user_count, 3, "prompt + steering + follow-up");
            let turn_starts = events
                .iter()
                .filter(|e| matches!(e, RichAgentEvent::TurnStart))
                .count();
            assert_eq!(turn_starts, 2);
        });
    }

    #[test]
    fn agent_class_steers_and_prompts() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("hello from agent")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn = scripted_stream(core);
            let agent = Agent::new(stream_fn);
            {
                let mut state = agent.state();
                state.model = model;
            }
            agent.prompt(steer_msg("hello")).await.unwrap();
            let msgs = {
                let s = agent.state();
                s.messages().to_vec()
            };
            assert!(msgs.iter().any(
                |m| matches!(m, AgentMessage::Core(Message::Assistant(a)) if !a.content().is_empty())
            ));
        });
    }

    #[test]
    fn shared_agent_state_reasoning_reaches_next_provider_config() {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        let mut agent = Agent::new(scripted_stream(core));

        agent.set_reasoning(Some(ThinkingLevel::Low));
        assert_eq!(agent.state().thinking_level, Some(ThinkingLevel::Low));

        let agent = Arc::new(agent);
        let shared_handle = agent.clone();
        agent.state().thinking_level = Some(ThinkingLevel::High);

        let mut skip_initial_steering = false;
        let config = agent.build_config(
            default_model(),
            &mut skip_initial_steering,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(config.reasoning, Some(ThinkingLevel::High));

        drop(shared_handle);
    }

    fn delayed_lifecycle_transport(
        responses: Vec<&str>,
        delay: std::time::Duration,
    ) -> (StreamFn, Arc<tokio::sync::Notify>, Arc<AtomicUsize>) {
        let responses = Arc::new(Mutex::new(
            responses
                .into_iter()
                .map(str::to_owned)
                .collect::<std::collections::VecDeque<_>>(),
        ));
        let started = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let transport = {
            let responses = responses.clone();
            let started = started.clone();
            let calls = calls.clone();
            Arc::new(move |model: &pi_ai::model::Model, _context: &Context| {
                let response = responses
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .pop_front()
                    .unwrap_or_else(|| "unexpected extra request".to_string());
                calls.fetch_add(1, Ordering::SeqCst);
                let stream = pi_ai::AssistantMessageEventStream::new();
                let sender = stream.sender().expect("delayed stream sender");
                let started = started.clone();
                let model = model.clone();
                tokio::spawn(async move {
                    started.notify_waiters();
                    tokio::time::sleep(delay).await;
                    let mut message = AssistantMessage::new();
                    message.set_api_provider_model(&model.api, &model.provider, &model.id);
                    message.set_content(vec![ContentBlock::text(response)]);
                    message.set_stop_reason(StopReason::Stop);
                    let _ = sender.send(AssistantMessageEvent::Start {
                        partial: message.clone(),
                    });
                    let _ = sender.send(AssistantMessageEvent::Done {
                        reason: pi_ai::types::DoneReason::Stop,
                        message,
                    });
                });
                stream
            }) as StreamFn
        };
        (transport, started, calls)
    }

    fn lifecycle_agent(transport: StreamFn) -> Arc<Agent> {
        let agent = Arc::new(Agent::new(transport));
        agent.state().model = pi_ai::model::Model::new("delayed-1", "Delayed", "delayed", "test");
        agent
    }

    fn user_text(message: &AgentMessage) -> Option<&str> {
        let AgentMessage::Core(Message::User(user)) = message else {
            return None;
        };
        match user.content() {
            pi_ai::types::UserContentBody::String(text) => Some(text.as_str()),
            pi_ai::types::UserContentBody::Blocks(blocks) => {
                blocks.iter().find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
            }
        }
    }

    #[test]
    fn active_run_abort_cancels_delayed_transport_and_clears_streaming_state() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, calls) = delayed_lifecycle_transport(
                vec!["never delivered"],
                std::time::Duration::from_secs(5),
            );
            let agent = lifecycle_agent(transport);
            let started_wait = started.notified();
            let running = agent.clone();
            let prompt = tokio::spawn(async move { running.prompt(steer_msg("abort me")).await });
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("delayed transport should start");
            assert!(agent.is_streaming());
            let signal = agent.signal().expect("active run signal");
            assert!(!signal.load(Ordering::SeqCst));

            agent.abort();
            assert!(signal.load(Ordering::SeqCst));
            assert!(
                agent.is_streaming(),
                "abort must not clear state before settlement"
            );

            tokio::time::timeout(std::time::Duration::from_secs(1), prompt)
                .await
                .expect("abort should not wait for the delayed response")
                .expect("prompt task should not panic")
                .expect("aborted prompt should settle successfully");
            assert!(!agent.is_streaming());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let state = agent.state();
            let assistant = state
                .messages()
                .iter()
                .rev()
                .find_map(|message| match message {
                    AgentMessage::Core(Message::Assistant(message)) => Some(message),
                    _ => None,
                })
                .expect("aborted run should record an assistant terminal message");
            assert_eq!(assistant.stop_reason(), Some(StopReason::Aborted));
        });
    }

    #[test]
    fn run_lease_clears_active_state_when_transport_panics() {
        let rt = rt();
        rt.block_on(async {
            let transport: StreamFn = Arc::new(|_, _| panic!("deterministic transport panic"));
            let agent = Arc::new(Agent::new(transport));
            let running = agent.clone();
            let task = tokio::spawn(async move { running.prompt(steer_msg("panic")).await });
            let result = task
                .await
                .expect("a transport panic must be converted into a terminal error message");
            assert!(
                result.is_ok(),
                "provider failures settle as an agent result"
            );
            assert!(
                !agent.is_streaming(),
                "RAII cleanup must clear a panicked run"
            );
            assert!(
                agent
                    .error_message()
                    .as_deref()
                    .is_some_and(|message| message.contains("deterministic transport panic")),
                "the converted provider error must remain observable"
            );
            agent.wait_for_idle().await;
        });
    }

    #[test]
    fn concurrent_prompt_rejects_and_wait_for_idle_includes_async_listeners() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, _calls) = delayed_lifecycle_transport(
                vec!["done"],
                std::time::Duration::from_millis(30),
            );
            let agent = lifecycle_agent(transport);
            let listener_started = Arc::new(tokio::sync::Notify::new());
            let listener_release = Arc::new(tokio::sync::Notify::new());
            let listener_wait = listener_started.notified();
            let listener_started_for_agent = listener_started.clone();
            let listener_release_for_agent = listener_release.clone();
            let _unsubscribe = agent.subscribe(move |event, _signal| {
                let listener_started = listener_started_for_agent.clone();
                let listener_release = listener_release_for_agent.clone();
                Box::pin(async move {
                    if matches!(event, RichAgentEvent::AgentEnd { .. }) {
                        listener_started.notify_waiters();
                        listener_release.notified().await;
                    }
                })
            });

            let started_wait = started.notified();
            let running = agent.clone();
            let first_prompt =
                tokio::spawn(async move { running.prompt(steer_msg("first")).await });
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("first transport should start");

            let second = agent.prompt(steer_msg("second")).await;
            assert_eq!(second, Err(AgentRunError::AlreadyProcessingPrompt));
            assert!(agent.is_streaming());
            tokio::time::timeout(std::time::Duration::from_secs(1), listener_wait)
                .await
                .expect("agent_end listener should be entered");

            let idle = agent.wait_for_idle();
            tokio::pin!(idle);
            tokio::select! {
                _ = &mut idle => panic!("wait_for_idle must remain pending while a listener is blocked"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
            assert!(agent.is_streaming(), "listener settlement is part of the run");
            listener_release.notify_one();
            first_prompt
                .await
                .expect("first prompt task should not panic")
                .expect("first prompt should complete");
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut idle)
                .await
                .expect("wait_for_idle should resolve after the listener");
            assert!(!agent.is_streaming());
        });
    }

    #[test]
    fn continue_rejects_while_active_and_abort_clears_signal() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, _calls) = delayed_lifecycle_transport(
                vec!["never delivered"],
                std::time::Duration::from_secs(5),
            );
            let agent = lifecycle_agent(transport);
            agent
                .state()
                .messages
                .push(steer_msg("continue from this user message"));

            let started_wait = started.notified();
            let running = agent.clone();
            let continuation = tokio::spawn(async move { running.continue_().await });
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("continuation transport should start");
            assert!(agent.is_streaming());
            assert_eq!(
                agent.continue_().await,
                Err(AgentRunError::AlreadyProcessingContinuation)
            );

            let signal = agent.signal().expect("active continuation signal");
            agent.abort();
            assert!(signal.load(Ordering::SeqCst));
            continuation
                .await
                .expect("continuation task should not panic")
                .expect("aborted continuation should settle successfully");
            assert!(!agent.is_streaming());
            assert!(agent.signal().is_none());
        });
    }

    #[test]
    fn continue_reports_empty_and_assistant_tail_validation_errors() {
        let rt = rt();
        rt.block_on(async {
            let (transport, _started, _calls) =
                delayed_lifecycle_transport(Vec::new(), std::time::Duration::from_millis(1));
            let agent = lifecycle_agent(transport);
            assert_eq!(
                agent.continue_().await,
                Err(AgentRunError::NoMessagesToContinue)
            );

            let model = agent.state().model.clone();
            agent
                .state()
                .messages
                .push(AgentMessage::Core(Message::Assistant(
                    aborted_assistant_message(&model),
                )));
            assert_eq!(
                agent.continue_().await,
                Err(AgentRunError::CannotContinueFromAssistant)
            );
        });
    }

    #[test]
    fn continue_from_assistant_tail_drains_steering_then_follow_up_queues() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, calls) = delayed_lifecycle_transport(
                vec!["steering result", "follow-up result"],
                std::time::Duration::from_millis(20),
            );
            let agent = lifecycle_agent(transport);
            let model = agent.state().model.clone();
            agent
                .state()
                .messages
                .push(AgentMessage::Core(Message::Assistant(
                    aborted_assistant_message(&model),
                )));
            agent.steer(steer_msg("queued steering"));
            agent.follow_up(steer_msg("queued follow-up"));

            let started_wait = started.notified();
            let running = agent.clone();
            let continuation = tokio::spawn(async move { running.continue_().await });
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("queued steering should start a transport turn");
            continuation
                .await
                .expect("continuation task should not panic")
                .expect("queued continuation should complete");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert!(!agent.has_queued_messages());
            let state = agent.state();
            let user_texts: Vec<&str> = state.messages().iter().filter_map(user_text).collect();
            assert_eq!(user_texts, vec!["queued steering", "queued follow-up"]);
            assert!(!agent.is_streaming());
        });
    }

    #[test]
    fn subscribers_receive_each_event_live_and_before_run_cleanup() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, calls) = delayed_lifecycle_transport(
                vec!["never delivered"],
                std::time::Duration::from_secs(5),
            );
            let agent = lifecycle_agent(transport);
            let start_seen = Arc::new(tokio::sync::Notify::new());
            let observed = Arc::new(Mutex::new(Vec::<RichAgentEvent>::new()));
            let callback_signal = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
            let start_seen_for_listener = start_seen.clone();
            let observed_for_listener = observed.clone();
            let callback_signal_for_listener = callback_signal.clone();
            let _unsubscribe = agent.subscribe(move |event, signal| {
                let start_seen = start_seen_for_listener.clone();
                let observed = observed_for_listener.clone();
                let callback_signal = callback_signal_for_listener.clone();
                Box::pin(async move {
                    if matches!(event, RichAgentEvent::AgentStart) {
                        *callback_signal
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = signal;
                        start_seen.notify_waiters();
                    }
                    observed
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(event);
                })
            });

            let started_wait = started.notified();
            let start_wait = start_seen.notified();
            let running = agent.clone();
            let prompt = tokio::spawn(async move {
                running
                    .prompt_messages_with_events(vec![steer_msg("live subscriber")])
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(1), start_wait)
                .await
                .expect("AgentStart subscriber should run before the delayed response settles");
            assert!(agent.is_streaming());
            assert!(!callback_signal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .expect("subscriber should receive the active signal")
                .load(Ordering::SeqCst));
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("delayed response should be active while the subscriber is live");

            agent.abort();
            let (messages, events) =
                tokio::time::timeout(std::time::Duration::from_secs(1), prompt)
                    .await
                    .expect("abort should settle the run")
                    .expect("prompt task should not panic")
                    .expect("aborted prompt should settle successfully");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(!agent.is_streaming());
            assert!(agent.signal().is_none());
            assert!(!messages.is_empty());

            let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
            assert_eq!(observed.len(), events.len());
            assert!(matches!(observed.first(), Some(RichAgentEvent::AgentStart)));
            assert!(matches!(
                observed.last(),
                Some(RichAgentEvent::AgentEnd { .. })
            ));
            assert!(observed.iter().any(|event| {
                matches!(
                    event,
                    RichAgentEvent::MessageEnd {
                        message: AgentMessage::Core(Message::Assistant(message))
                    } if message.stop_reason() == Some(StopReason::Aborted)
                )
            }));
        });
    }

    #[test]
    fn steering_and_follow_up_queued_during_streaming_reach_distinct_turns() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, calls) = delayed_lifecycle_transport(
                vec!["first", "second", "third"],
                std::time::Duration::from_millis(20),
            );
            let agent = lifecycle_agent(transport);
            let started_wait = started.notified();
            let running = agent.clone();
            let prompt =
                tokio::spawn(
                    async move { running.prompt_messages(vec![steer_msg("initial")]).await },
                );
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("first transport should start");
            agent.steer(steer_msg("steer while streaming"));
            agent.follow_up(steer_msg("follow after stop"));

            let messages = prompt
                .await
                .expect("prompt task should not panic")
                .expect("prompt should complete");
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            let user_texts: Vec<&str> = messages.iter().filter_map(user_text).collect();
            assert_eq!(
                user_texts,
                vec!["initial", "steer while streaming", "follow after stop"]
            );
            assert!(!agent.has_queued_messages());
            assert!(!agent.is_streaming());
        });
    }

    #[test]
    fn all_queue_mode_drains_all_live_steering_messages_at_one_boundary() {
        let rt = rt();
        rt.block_on(async {
            let (transport, started, calls) = delayed_lifecycle_transport(
                vec!["first", "second"],
                std::time::Duration::from_millis(20),
            );
            let agent = lifecycle_agent(transport);
            agent.set_steering_mode(QueueMode::All);
            let started_wait = started.notified();
            let running = agent.clone();
            let prompt =
                tokio::spawn(
                    async move { running.prompt_messages(vec![steer_msg("initial")]).await },
                );
            tokio::time::timeout(std::time::Duration::from_secs(1), started_wait)
                .await
                .expect("first transport should start");
            agent.steer(steer_msg("steer one"));
            agent.steer(steer_msg("steer two"));

            let messages = prompt
                .await
                .expect("prompt task should not panic")
                .expect("prompt should complete");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let user_texts: Vec<&str> = messages.iter().filter_map(user_text).collect();
            assert_eq!(user_texts, vec!["initial", "steer one", "steer two"]);
            assert!(!agent.has_queued_messages());
        });
    }
}
