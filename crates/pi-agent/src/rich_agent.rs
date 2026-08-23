//! Rich agent surface — additive port of `packages/agent/src/agent.ts` +
//! `agent-loop.ts` on top of the existing tool contract.
//!
//! Provides the upstream-shaped loop (`PendingMessageQueue`, `QueueMode`,
//! `ToolExecutionMode`), a full event stream (`message_update` /
//! `tool_execution_*`), steering/follow-up drains, before/afterToolCall hooks,
//! sequential+parallel tool batches, and the stateful `Agent` class.
//!
//! Known divergence (tracked against the AgentTool contract upgrade):
//! - `tool_execution_update` is not emitted because built-in tools do not yet
//!   stream partial results through the execution closure.
//! - Tool `terminate` hints are not representable on `ToolResultMessage`;
//!   batch early-termination always resolves to false (upstream's behavior
//!   when no tool sets `terminate`).
//! - `validateToolArguments` is performed by the rely-on-the-tool schema
//!   contract; the pi-ai validator port will bolt on when it lands.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, Context, Message, StopReason, ThinkingLevel,
    ToolResultMessage,
};

use crate::agent::{is_aborted, AgentContext, StreamFn};
use crate::tools::AgentTool;
use crate::types::AgentMessage;

/// How queued messages are injected at a drain point (upstream `QueueMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

impl Default for QueueMode {
    fn default() -> Self {
        QueueMode::OneAtATime
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

impl Default for ToolExecutionMode {
    fn default() -> Self {
        ToolExecutionMode::Parallel
    }
}

/// Events emitted by the `Agent` for UI updates (upstream `AgentEvent`).
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
        result: ToolResultMessage,
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
    pub is_error: Option<bool>,
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

pub type BeforeToolCallHook = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<Arc<AtomicBool>>,
        ) -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>
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
    /// Optional transform applied at the AgentMessage level before conversion.
    pub transform_context:
        Option<AsyncHook<(Vec<AgentMessage>, Option<Arc<AtomicBool>>), Vec<AgentMessage>>>,
    /// Dynamic API key resolver for each LLM call.
    pub get_api_key: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
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

fn create_error_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    message: &str,
) -> ToolResultMessage {
    ToolResultMessage::text(tool_call_id, tool_name, message, true)
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
        details: Some(result.details.clone()),
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

async fn drain_steering(config: &RichAgentLoopConfig) -> Vec<AgentMessage> {
    (config.get_steering_messages)(()).await
}

async fn drain_follow_up(config: &RichAgentLoopConfig) -> Vec<AgentMessage> {
    (config.get_follow_up_messages)(()).await
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
        let result = create_error_tool_result(
            tc.id,
            tc.name,
            &format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tc.name
            ),
        );
        emit(RichAgentEvent::ToolExecutionEnd {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            result: result.clone(),
            is_error: true,
        });
        emit_tool_result_messages(&mut messages, result, emit);
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
    if config.tool_execution == ToolExecutionMode::Sequential || tool_calls.len() <= 1 {
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
    for tc in tool_calls_of(message) {
        emit(RichAgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            args: tc.arguments.clone(),
        });
        match prepare_tool_call(message, &tc, context, config).await {
            PreparedToolCall::Immediate { result, is_error } => {
                emit(RichAgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.to_string(),
                    tool_name: tc.name.to_string(),
                    result: result.clone(),
                    is_error,
                });
                emit_tool_result_messages(&mut messages, result, emit);
                terminate_flags.push(false);
            }
            PreparedToolCall::Prepared { tool, args } => {
                let (result, is_error) =
                    execute_tool_once(&tc, &tool, args.clone(), config.signal.as_ref()).await;
                let terminate = result.terminate;
                let (result, is_error) =
                    finalize_tool_call(message, &tc, args, result, is_error, config, emit).await;
                let message_result =
                    agent_tool_result_to_message(&tc.id, &tc.name, &result, is_error);
                emit(RichAgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.to_string(),
                    tool_name: tc.name.to_string(),
                    result: message_result.clone(),
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
    let mut prepared: Vec<Option<PreparedToolCall>> = Vec::new();
    for tc in tool_calls_of(message) {
        emit(RichAgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.to_string(),
            tool_name: tc.name.to_string(),
            args: tc.arguments.clone(),
        });
        prepared.push(Some(prepare_tool_call(message, &tc, context, config).await));
        if is_aborted(config.signal.as_ref()) {
            break;
        }
    }

    // Phase 2: execute prepared calls concurrently, then finalize in order.
    let calls: Vec<(String, String, serde_json::Value, usize)> = tool_calls_of(message)
        .into_iter()
        .enumerate()
        .filter_map(|(i, tc)| match &prepared.get(i) {
            Some(Some(PreparedToolCall::Prepared { .. })) => Some((
                tc.id.to_string(),
                tc.name.to_string(),
                tc.arguments.clone(),
                i,
            )),
            _ => None,
        })
        .collect();

    let tools: Vec<AgentTool> = context.tools.clone();
    let signal = config.signal.clone();

    let mut finalized: Vec<(usize, (crate::tools::AgentToolResult, bool))> = Vec::new();
    if !calls.is_empty() {
        let futures: Vec<_> = calls
            .iter()
            .map(|(id, name, args, i)| {
                let id = id.clone();
                let name = name.clone();
                let args = args.clone();
                let i = *i;
                let tools = tools.clone();
                let signal = signal.clone();
                async move {
                    let tool = tools.iter().find(|t| t.tool.name == name).cloned();
                    let (result, is_error) = match tool {
                        Some(tool) => {
                            let tc = ToolCallRef {
                                id: &id,
                                name: &name,
                                arguments: &args,
                            };
                            execute_tool_once(&tc, &tool, args.clone(), signal.as_ref()).await
                        }
                        None => (
                            crate::tools::AgentToolResult::text(format!("Tool {name} not found")),
                            true,
                        ),
                    };
                    (i, (result, is_error))
                }
                .boxed()
            })
            .collect();
        let outputs = futures_util::future::join_all(futures).await;
        finalized.extend(outputs);
    }

    // Emit tool_execution_end + tool-result messages in assistant source order.
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    let mut emitted: Vec<(String, String)> = Vec::new();
    let mut terminate_flags: Vec<bool> = Vec::new();
    for (i, (result, is_error)) in finalized {
        let (id, name) = calls
            .get(i)
            .map(|c| (c.0.clone(), c.1.clone()))
            .unwrap_or_default();
        if emitted.iter().any(|(eid, _)| eid == &id) {
            continue;
        }
        emitted.push((id.clone(), name.clone()));
        terminate_flags.push(result.terminate);
        let message_result = agent_tool_result_to_message(&id, &name, &result, is_error);
        emit(RichAgentEvent::ToolExecutionEnd {
            tool_call_id: id,
            tool_name: name,
            result: message_result.clone(),
            is_error,
        });
        emit_tool_result_messages(&mut messages, message_result, emit);
    }
    // Immediate (error) preparations in source order too.
    for (i, entry) in prepared.iter().enumerate() {
        if let Some(PreparedToolCall::Immediate { result, is_error }) = entry {
            let Some((id, name, _, _)) = calls.iter().find(|(_, _, _, idx)| *idx == i) else {
                // Immediate from a call that wasn't executed (aborted mid-batch).
                continue;
            };
            if emitted.iter().any(|(eid, _)| eid == id) {
                continue;
            }
            emitted.push((id.clone(), name.clone()));
            emit(RichAgentEvent::ToolExecutionEnd {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                result: result.clone(),
                is_error: *is_error,
            });
            emit_tool_result_messages(&mut messages, result.clone(), emit);
        }
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
        result: ToolResultMessage,
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
            result: create_error_tool_result(
                tc.id,
                tc.name,
                &format!("Tool {} not found", tc.name),
            ),
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
    let validated = match crate::tools::validation::validate_tool_arguments(
        tc.name,
        &tool.tool.parameters,
        &args,
    ) {
        Ok(validated) => validated,
        Err(e) => {
            return PreparedToolCall::Immediate {
                result: create_error_tool_result(tc.id, tc.name, &e),
                is_error: true,
            };
        }
    };
    if let Some(hook) = &config.before_tool_call {
        let before = hook(
            BeforeToolCallContext {
                assistant_message: message.clone(),
                tool_call_id: tc.id.to_string(),
                tool_name: tc.name.to_string(),
                args: validated.clone(),
            },
            config.signal.clone(),
        )
        .await;
        if is_aborted(config.signal.as_ref()) {
            return PreparedToolCall::Immediate {
                result: create_error_tool_result(tc.id, tc.name, "Operation aborted"),
                is_error: true,
            };
        }
        if let Some(before) = before {
            if before.block {
                let reason = before
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".to_string());
                return PreparedToolCall::Immediate {
                    result: create_error_tool_result(tc.id, tc.name, &reason),
                    is_error: true,
                };
            }
        }
    }
    if is_aborted(config.signal.as_ref()) {
        return PreparedToolCall::Immediate {
            result: create_error_tool_result(tc.id, tc.name, "Operation aborted"),
            is_error: true,
        };
    }
    PreparedToolCall::Prepared {
        tool: tool.clone(),
        args: validated,
    }
}

async fn execute_tool_once(
    tc: &ToolCallRef<'_>,
    tool: &AgentTool,
    args: serde_json::Value,
    signal: Option<&Arc<AtomicBool>>,
) -> (crate::tools::AgentToolResult, bool) {
    if is_aborted(signal) {
        return (
            crate::tools::AgentToolResult::text("Operation aborted"),
            true,
        );
    }
    match (tool.execute)(tc.id.to_string(), args, signal.cloned(), None).await {
        Ok(result) => {
            let is_error = false;
            (result, is_error)
        }
        Err(e) => (crate::tools::AgentToolResult::text(e), true),
    }
}

/// afterToolCall merge (upstream `finalizeExecutedToolCall`).
async fn finalize_tool_call<F>(
    message: &AssistantMessage,
    tc: &ToolCallRef<'_>,
    args: serde_json::Value,
    result: crate::tools::AgentToolResult,
    is_error: bool,
    config: &RichAgentLoopConfig,
    emit: &mut F,
) -> (crate::tools::AgentToolResult, bool)
where
    F: FnMut(RichAgentEvent) + Send,
{
    let _ = emit;
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
    let llm_messages: Vec<Message> = match &config.convert_to_llm {
        Some(convert) => convert(&messages),
        None => crate::messages::convert_to_llm(&messages),
    };

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
    let mut final_message = stream
        .for_each(|event| match &event {
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
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {}
        })
        .await;
    if is_aborted(config.signal.as_ref()) {
        final_message.set_stop_reason(StopReason::Aborted);
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
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {}
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
    }
}

/// Stateful wrapper around the low-level agent loop (upstream `Agent`).
pub struct Agent {
    state: Mutex<AgentState>,
    listeners: Mutex<
        Vec<
            Arc<
                dyn Fn(
                        RichAgentEvent,
                        Option<Arc<AtomicBool>>,
                    ) -> Pin<Box<dyn Future<Output = ()> + Send>>
                    + Send
                    + Sync,
            >,
        >,
    >,
    steering_queue: Mutex<PendingMessageQueue>,
    follow_up_queue: Mutex<PendingMessageQueue>,
    active_run: Mutex<Option<ActiveRun>>,
    convert_to_llm: Option<ConvertToLlmFn>,
    stream_fn: StreamFn,
    before_tool_call: Option<BeforeToolCallHook>,
    after_tool_call: Option<AfterToolCallHook>,
    should_stop_after_turn: Option<ShouldStopAfterTurnHook>,
    tool_execution: ToolExecutionMode,
    session_id: Option<String>,
    reasoning: Option<ThinkingLevel>,
}

struct ActiveRun {
    abort: Arc<AtomicBool>,
}

impl Agent {
    pub fn new(stream_fn: StreamFn) -> Self {
        Self {
            state: Mutex::new(AgentState::default()),
            listeners: Mutex::new(Vec::new()),
            steering_queue: Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime)),
            follow_up_queue: Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime)),
            active_run: Mutex::new(None),
            convert_to_llm: None,
            stream_fn,
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            tool_execution: ToolExecutionMode::Parallel,
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

    pub fn is_streaming(&self) -> bool {
        self.active_run.lock().unwrap().is_some()
    }

    /// Start a prompt run (upstream `Agent.prompt`). Returns after settlement
    /// of the run and its awaited listeners.
    pub async fn prompt(&self, message: AgentMessage) {
        self.run_prompt_messages(vec![message], false).await;
    }

    /// Continue from the current transcript (upstream `Agent.continue`).
    pub async fn continue_(&self) {
        let last = {
            let state = self.state.lock().unwrap();
            state.messages.last().cloned()
        };
        let Some(last) = last else {
            panic!("No messages to continue from");
        };
        if last.role() == "assistant" {
            let queued_steering = self.steering_queue.lock().unwrap().drain();
            if !queued_steering.is_empty() {
                self.run_prompt_messages(queued_steering, true).await;
                return;
            }
            let queued_follow_ups = self.follow_up_queue.lock().unwrap().drain();
            if !queued_follow_ups.is_empty() {
                self.run_prompt_messages(queued_follow_ups, false).await;
                return;
            }
            panic!("Cannot continue from message role: assistant");
        }
        self.run_continuation().await;
    }

    async fn run_prompt_messages(&self, messages: Vec<AgentMessage>, skip_initial_steering: bool) {
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
        {
            let state = self.state.lock().unwrap();
            context.messages = state.messages.clone();
        }
        let config = self.build_config(model, &mut skip);
        let mut events: Vec<RichAgentEvent> = Vec::new();
        let new_messages = run_rich_agent_loop(messages, &mut context, &config, &mut |e| {
            events.push(e.clone())
        })
        .await;
        self.record_events(&events).await;
        {
            let mut state = self.state.lock().unwrap();
            state.messages = context.messages;
        }
        let _ = new_messages;
    }

    async fn run_continuation(&self) {
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
        let config = self.build_config(model, &mut false);
        let mut events: Vec<RichAgentEvent> = Vec::new();
        let _new_messages = run_rich_agent_loop(Vec::new(), &mut context, &config, &mut |e| {
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
    ) -> RichAgentLoopConfig {
        let mut config = RichAgentLoopConfig::new(model, self.stream_fn.clone(), None);
        config.convert_to_llm = self.convert_to_llm.clone();
        config.before_tool_call = self.before_tool_call.clone();
        config.after_tool_call = self.after_tool_call.clone();
        config.should_stop_after_turn = self.should_stop_after_turn.clone();
        config.tool_execution = self.tool_execution;
        config.session_id = self.session_id.clone();
        config.reasoning = self.reasoning;
        let steer = self.steering_queue.lock().unwrap().clone();
        let initial_skip = *skip_initial_steering;
        let poll_count = Arc::new(AtomicUsize::new(0));
        let steering_hook: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
            let mut steer = steer.clone();
            let poll_count = poll_count.clone();
            async move {
                // Upstream skips only the initial steering poll in
                // `runPromptMessages({ skipInitialSteeringPoll: true })`.
                if initial_skip && poll_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Vec::new();
                }
                steer.drain()
            }
            .boxed()
        });
        let follow = self.follow_up_queue.lock().unwrap().clone();
        let follow_up_hook: AsyncHook<(), Vec<AgentMessage>> = Arc::new(move |()| {
            let mut follow = follow.clone();
            async move { follow.drain() }.boxed()
        });
        config.get_steering_messages = steering_hook;
        config.get_follow_up_messages = follow_up_hook;
        config
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
            assert!(events.iter().any(|e| matches!(e, RichAgentEvent::AgentEnd { .. })));
            let has_tool_result = new_messages.iter().any(|m| {
                matches!(m, AgentMessage::Core(Message::ToolResult(r)) if !r.is_error())
            });
            assert!(has_tool_result);
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
                        ..Default::default()
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
                    ..Default::default()
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
            agent.prompt(steer_msg("hello")).await;
            let msgs = {
                let s = agent.state();
                s.messages().to_vec()
            };
            assert!(msgs.iter().any(
                |m| matches!(m, AgentMessage::Core(Message::Assistant(a)) if a.content().len() > 0)
            ));
        });
    }
}
