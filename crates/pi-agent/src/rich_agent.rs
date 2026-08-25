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

use crate::agent::{is_aborted, AgentContext, StreamFn};
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
pub struct RichAgentLoopConfig {
    pub model: pi_ai::model::Model,
    /// Provider stream function.
    pub stream_fn: StreamFn,
    /// Abort flag for the run.
    pub signal: Option<Arc<AtomicBool>>,
    /// Convert `AgentMessage[]` to LLM `Message[]` before each call. Defaults
    /// to the harness converter (custom messages rendered for the provider).
    pub convert_to_llm: Option<ConvertToLlmFn>,
    /// Replace image blocks at the provider boundary while keeping them in
    /// the durable transcript/UI result.
    pub block_images: bool,
    /// Optional transform applied at the AgentMessage level before conversion.
    pub transform_context: Option<TransformContextHook>,
    /// Dynamic API key resolver for each LLM call.
    pub get_api_key: Option<ApiKeyResolver>,
    /// Reasoning level forwarded to the stream function.
    pub reasoning: Option<ThinkingLevel>,
    /// Session identifier forwarded to cache-aware providers.
    pub session_id: Option<String>,
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
            signal,
            convert_to_llm: None,
            block_images: false,
            transform_context: None,
            get_api_key: None,
            reasoning: None,
            session_id: None,
            max_retry_delay_ms: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
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
    (config.get_steering_messages)(()).await
}

async fn drain_follow_up(config: &RichAgentLoopConfig) -> Vec<AgentMessage> {
    (config.get_follow_up_messages)(()).await
}

async fn wait_for_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
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
        let prepared = prepare(args.clone());
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
        let before = hook(&mut before_context, config.signal.clone()).await;
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
        let execution = (tool.execute)(tool_call_id, args, signal.clone(), Some(on_update)).await;
        accepting_updates.store(false, Ordering::Release);
        match execution {
            Ok(result) => (result, false),
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
    let result = loop {
        tokio::select! {
            update = receiver.recv() => {
                if let Some(update) = update {
                    emit_tool_update(update, emit);
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
    result: crate::tools::AgentToolResult,
    is_error: bool,
    config: &RichAgentLoopConfig,
) -> (crate::tools::AgentToolResult, bool) {
    let Some(hook) = &config.after_tool_call else {
        return (result, is_error);
    };
    let after = hook(
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
    .await;
    let Some(after) = after else {
        return (result, is_error);
    };
    let mut result = result;
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
    let is_error = after.is_error.unwrap_or(is_error);
    (result, is_error)
}

/// Run the rich agent loop (upstream `runAgentLoop`).
pub async fn run_rich_agent_loop<F>(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> Vec<AgentMessage>
where
    F: FnMut(RichAgentEvent) + Send,
{
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_messages: Vec<AgentMessage> = context
        .messages
        .iter()
        .cloned()
        .chain(prompts.clone())
        .collect();

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
    let mut pending_messages: Vec<AgentMessage> = drain_steering(config).await;

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

            let message = stream_assistant_response(&current_messages, context, config, emit).await;
            current_messages.push(AgentMessage::Core(Message::Assistant(message.clone())));
            context
                .messages
                .push(AgentMessage::Core(Message::Assistant(message.clone())));
            new_messages.push(AgentMessage::Core(Message::Assistant(message.clone())));

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
                    execute_tool_batch(&message, context, config, emit).await
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
                if batch.terminate {
                    emit(RichAgentEvent::TurnEnd {
                        message: AgentMessage::Core(Message::Assistant(message.clone())),
                        tool_results: tool_results.clone(),
                    });
                    emit(RichAgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    });
                    return new_messages;
                }
            }

            emit(RichAgentEvent::TurnEnd {
                message: AgentMessage::Core(Message::Assistant(message.clone())),
                tool_results: tool_results.clone(),
            });

            let should_stop = match &config.should_stop_after_turn {
                Some(hook) => hook(message, tool_results.clone()).await,
                None => false,
            };
            if should_stop {
                emit(RichAgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return new_messages;
            }

            pending_messages = drain_steering(config).await;
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_ups = drain_follow_up(config).await;
        if !follow_ups.is_empty() {
            pending_messages = follow_ups;
            continue 'outer;
        }
        break;
    }

    emit(RichAgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
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
        let mut aborted = AssistantMessage::new();
        aborted.set_stop_reason(StopReason::Aborted);
        return aborted;
    }

    // Apply context transform if configured (AgentMessage[] -> AgentMessage[]).
    let mut messages = current_messages.to_vec();
    if let Some(transform) = &config.transform_context {
        messages = transform((messages, config.signal.clone())).await;
    }

    // Convert to LLM-compatible messages.
    let mut llm_messages: Vec<Message> = match &config.convert_to_llm {
        Some(convert) => convert(&messages),
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

    // Resolve API key (expiring tokens for short-lived OAuth providers).
    let _resolved_api_key = config
        .get_api_key
        .as_ref()
        .and_then(|f| f(&config.model.provider))
        .or_else(|| config.api_key.clone());

    if let Some(policy) = &config.retry_policy {
        // Retry is implemented at the assistant-call boundary, but every
        // attempt still follows the normal rich message lifecycle. Keep the
        // events in a shared ordered buffer while the retry helper sleeps and
        // invokes later attempts; the caller can then emit the complete
        // attempt history in the same order it was observed.
        let retry_events = Arc::new(Mutex::new(Vec::<RichAgentEvent>::new()));
        let stream_fn = config.stream_fn.clone();
        let model = config.model.clone();
        let retry_context = llm_context.clone();
        let retry_signal = config
            .retry_signal
            .clone()
            .or_else(|| config.signal.clone());
        let scheduled_events = retry_events.clone();
        let finished_events = retry_events.clone();
        let callback_signal = retry_signal.clone();
        let callback_run_signal = config.signal.clone();
        let callbacks = pi_ai::utils::retry::RetryCallbacks {
            on_retry_scheduled: Some(Box::new(move |attempt, max_attempts, delay_ms, error| {
                scheduled_events
                    .lock()
                    .unwrap()
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
                    .unwrap()
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
                let stream = stream_fn(&model, &retry_context);
                let signal = config.signal.clone();
                let producer_events = producer_events.clone();
                async move { stream_assistant_attempt(stream, signal, producer_events).await }
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
            if let Some(error) = retry_events.lock().unwrap().iter().rev().find_map(|event| {
                let RichAgentEvent::MessageEnd { message } = event else {
                    return None;
                };
                let AgentMessage::Core(Message::Assistant(message)) = message else {
                    return None;
                };
                (message.stop_reason() == Some(StopReason::Error)).then(|| message.clone())
            }) {
                final_message = error;
            }
        } else if is_aborted(config.signal.as_ref())
            && !retry_events.lock().unwrap().iter().any(|event| {
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
                .unwrap()
                .push(RichAgentEvent::MessageEnd {
                    message: AgentMessage::Core(Message::Assistant(final_message.clone())),
                });
        }

        let buffered = std::mem::take(&mut *retry_events.lock().unwrap());
        for event in buffered {
            emit(event);
        }
        if is_aborted(config.signal.as_ref()) {
            final_message.set_stop_reason(StopReason::Aborted);
        }
        return final_message;
    }

    let stream = (config.stream_fn)(&config.model, &llm_context);

    let emit_ref: &mut (dyn FnMut(RichAgentEvent) + Send) = emit;
    let mut stream_future = Box::pin(stream.for_each(|event| match &event {
        AssistantMessageEvent::Start { partial } => {
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

    if aborted {
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
) -> AssistantMessage {
    let mut added_partial = false;
    let events_for_stream = events.clone();
    let mut final_message = stream
        .for_each(|event| match &event {
            AssistantMessageEvent::Start { partial } => {
                added_partial = true;
                events_for_stream
                    .lock()
                    .unwrap()
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
                        .unwrap()
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
                    .unwrap()
                    .push(RichAgentEvent::MessageUpdate {
                        message: AgentMessage::Core(Message::Assistant(message.clone())),
                        assistant_message_event: event.clone(),
                    });
            }
        })
        .await;

    if is_aborted(signal.as_ref()) {
        final_message.set_stop_reason(StopReason::Aborted);
    }
    let final_agent_message = AgentMessage::Core(Message::Assistant(final_message.clone()));
    if !added_partial {
        events.lock().unwrap().push(RichAgentEvent::MessageStart {
            message: final_agent_message.clone(),
        });
    }
    events.lock().unwrap().push(RichAgentEvent::MessageEnd {
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
    state: Mutex<AgentState>,
    listeners: Mutex<Vec<AgentListener>>,
    steering_queue: Arc<Mutex<PendingMessageQueue>>,
    follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    active_run: Arc<Mutex<Option<ActiveRun>>>,
    convert_to_llm: Option<ConvertToLlmFn>,
    stream_fn: StreamFn,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
    should_stop_after_turn: Option<ShouldStopAfterTurnHook>,
    tool_execution: ToolExecutionMode,
    block_images: bool,
    session_id: Option<String>,
    reasoning: Option<ThinkingLevel>,
}

struct ActiveRun {
    abort: Arc<AtomicBool>,
    done: Arc<tokio::sync::Notify>,
}

struct RunLease {
    active_run: Arc<Mutex<Option<ActiveRun>>>,
    done: Arc<tokio::sync::Notify>,
}

impl Drop for RunLease {
    fn drop(&mut self) {
        let finished = {
            let mut active = self.active_run.lock().unwrap();
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
            self.done.notify_waiters();
        }
    }
}

impl Agent {
    pub fn new(stream_fn: StreamFn) -> Self {
        Self {
            state: Mutex::new(AgentState::default()),
            listeners: Mutex::new(Vec::new()),
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            active_run: Arc::new(Mutex::new(None)),
            convert_to_llm: None,
            stream_fn,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            tool_execution: ToolExecutionMode::Parallel,
            block_images: false,
            session_id: None,
            reasoning: None,
        }
    }

    pub fn set_convert_to_llm(&mut self, f: ConvertToLlmFn) {
        self.convert_to_llm = Some(f);
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
    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id;
    }
    pub fn set_reasoning(&mut self, level: Option<ThinkingLevel>) {
        self.reasoning = level;
    }
    pub fn set_tool_execution(&mut self, mode: ToolExecutionMode) {
        self.tool_execution = mode;
    }
    pub fn set_block_images(&mut self, block_images: bool) {
        self.block_images = block_images;
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, AgentState> {
        self.state.lock().unwrap()
    }

    /// Subscribe to lifecycle events (upstream `Agent.subscribe`).
    pub fn subscribe<F>(&self, listener: F)
    where
        F: Fn(RichAgentEvent, Option<Arc<AtomicBool>>) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.listeners.lock().unwrap().push(Arc::new(listener));
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.steering_queue.lock().unwrap().mode
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.steering_queue.lock().unwrap().mode = mode;
    }
    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up_queue.lock().unwrap().mode
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.follow_up_queue.lock().unwrap().mode = mode;
    }

    /// Queue a message to be injected after the current assistant turn
    /// finishes (upstream `Agent.steer`).
    pub fn steer(&self, message: AgentMessage) {
        self.steering_queue.lock().unwrap().enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop
    /// (upstream `Agent.followUp`).
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up_queue.lock().unwrap().enqueue(message);
    }

    pub fn clear_steering_queue(&self) {
        self.steering_queue.lock().unwrap().clear();
    }
    pub fn clear_follow_up_queue(&self) {
        self.follow_up_queue.lock().unwrap().clear();
    }
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue.lock().unwrap().has_items()
            || self.follow_up_queue.lock().unwrap().has_items()
    }

    /// Abort the current run, if one is active (upstream `Agent.abort`).
    pub fn abort(&self) {
        if let Some(run) = &*self.active_run.lock().unwrap() {
            run.abort.store(true, Ordering::SeqCst);
        }
    }

    /// Return the cancellation flag for the active run, if one exists.
    pub fn signal(&self) -> Option<Arc<AtomicBool>> {
        self.active_run
            .lock()
            .unwrap()
            .as_ref()
            .map(|run| run.abort.clone())
    }

    pub fn is_streaming(&self) -> bool {
        self.active_run.lock().unwrap().is_some()
    }

    /// Wait until the current run and all of its awaited listeners settle.
    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self
                .active_run
                .lock()
                .unwrap()
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
            let state = self.state.lock().unwrap();
            state.messages.last().cloned()
        };
        let Some(last) = last else {
            return Err(AgentRunError::NoMessagesToContinue);
        };
        if last.role() == "assistant" {
            let queued_steering = self.steering_queue.lock().unwrap().drain();
            if !queued_steering.is_empty() {
                self.run_prompt_messages(queued_steering, true)
                    .await
                    .map(|_| ())?;
                return Ok(());
            }
            let queued_follow_ups = self.follow_up_queue.lock().unwrap().drain();
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

    async fn run_prompt_messages_with_events_inner(
        &self,
        messages: Vec<AgentMessage>,
        skip_initial_steering: bool,
        signal: Arc<AtomicBool>,
    ) -> (Vec<AgentMessage>, Vec<RichAgentEvent>) {
        let mut skip = skip_initial_steering;
        let (model, system_prompt, tools) = {
            let state = self.state.lock().unwrap();
            (
                state.model.clone(),
                state.system_prompt.clone(),
                state.tools.clone(),
            )
        };
        let mut context = AgentContext::new(Some(system_prompt), tools);
        let prior_messages = {
            let state = self.state.lock().unwrap();
            state.messages.clone()
        };
        context.messages = prior_messages.clone();
        let config = self.build_config(model, &mut skip, signal);
        let mut events: Vec<RichAgentEvent> = Vec::new();
        let new_messages = run_rich_agent_loop(messages, &mut context, &config, &mut |e| {
            self.apply_event(&e);
            events.push(e.clone())
        })
        .await;
        self.record_events(&events).await;
        // `run_rich_agent_loop` keeps prompts in its local current-message
        // view, while the stateful Agent must retain them in its durable
        // transcript as well. The returned delta contains the prompts,
        // steering/follow-up messages, and all assistant/tool messages in
        // their durable order.
        context.messages = prior_messages;
        context.messages.extend(new_messages.clone());
        {
            let mut state = self.state.lock().unwrap();
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

    async fn run_continuation_inner(&self, signal: Arc<AtomicBool>) {
        let (model, system_prompt, tools) = {
            let state = self.state.lock().unwrap();
            (
                state.model.clone(),
                state.system_prompt.clone(),
                state.tools.clone(),
            )
        };
        let mut context = AgentContext::new(Some(system_prompt), tools);
        {
            let state = self.state.lock().unwrap();
            context.messages = state.messages.clone();
        }
        let config = self.build_config(model, &mut false, signal);
        let mut events: Vec<RichAgentEvent> = Vec::new();
        let _new_messages = run_rich_agent_loop(Vec::new(), &mut context, &config, &mut |e| {
            self.apply_event(&e);
            events.push(e.clone())
        })
        .await;
        self.record_events(&events).await;
        {
            let mut state = self.state.lock().unwrap();
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
        config.convert_to_llm = self.convert_to_llm.clone();
        config.block_images = self.block_images;
        config.before_tool_call = self.before_tool_call.clone();
        config.after_tool_call = self.after_tool_call.clone();
        config.should_stop_after_turn = self.should_stop_after_turn.clone();
        config.tool_execution = self.tool_execution;
        config.session_id = self.session_id.clone();
        config.reasoning = self.reasoning;
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
                steer.lock().unwrap().drain()
            }
            .boxed()
        });
        let follow = Arc::clone(&self.follow_up_queue);
        let follow_up_hook: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
            let follow = Arc::clone(&follow);
            async move { follow.lock().unwrap().drain() }.boxed()
        });
        config.get_steering_messages = steering_hook;
        config.get_follow_up_messages = follow_up_hook;
        config.retry_signal = Some(signal);
        config
    }

    fn apply_event(&self, event: &RichAgentEvent) {
        if let RichAgentEvent::MessageEnd { message } = event {
            self.state.lock().unwrap().messages.push(message.clone());
        }
    }

    fn begin_run(
        &self,
        already_running: AgentRunError,
    ) -> Result<(Arc<AtomicBool>, RunLease), AgentRunError> {
        let mut active = self.active_run.lock().unwrap();
        if active.is_some() {
            return Err(already_running);
        }
        let abort = Arc::new(AtomicBool::new(false));
        let done = Arc::new(tokio::sync::Notify::new());
        *active = Some(ActiveRun {
            abort: abort.clone(),
            done: done.clone(),
        });
        Ok((
            abort,
            RunLease {
                active_run: self.active_run.clone(),
                done,
            },
        ))
    }

    async fn record_events(&self, events: &[RichAgentEvent]) {
        let listeners = self.listeners.lock().unwrap().clone();
        let signal = self
            .active_run
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.abort.clone());
        for event in events {
            for listener in &listeners {
                listener(event.clone(), signal.clone()).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::providers::{
        faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
        RegisterFauxProviderOptions,
    };
    use pi_ai::types::ContentBlock;

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
            cfg.after_tool_call = Some(Arc::new(|context, _| {
                Box::pin(async move {
                    assert_eq!(context.tool_name, "echo");
                    Some(AfterToolCallResult {
                        content: Some(vec![ContentBlock::text("overridden")]),
                        terminate: Some(true),
                        ..Default::default()
                    })
                })
            }));
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
                    observed_by_tool.lock().unwrap().push(args["value"].clone());
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
            assert_eq!(*observed.lock().unwrap(), vec![serde_json::json!(123)]);
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
                    .unwrap()
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
            assert!(task.await.is_err(), "the test transport should panic");
            assert!(
                !agent.is_streaming(),
                "RAII cleanup must clear a panicked run"
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
            agent.subscribe(move |event, _signal| {
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
