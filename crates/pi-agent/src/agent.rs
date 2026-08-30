//! Agent loop — port of `packages/agent/src/agent-loop.ts` (the core
//! runLoop shape: turn streaming, tool execution, steering/follow-up
//! handling, termination signs). The harness-level hooks (compaction,
//! extensions, memory) layer on at the coding-agent level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::FutureExt;
use pi_ai::types::{
    AssistantMessage, Context, Message, StopReason, ToolResultMessage, UserContentBody,
};
use pi_ai::AssistantMessageEventStream;

use crate::tools::AgentTool;
use crate::types::AgentMessage;

/// Provider stream function: `(model, context) -> event stream`.
pub type StreamFn =
    Arc<dyn Fn(&pi_ai::model::Model, &Context) -> AssistantMessageEventStream + Send + Sync>;
/// Option-aware provider stream function. This additive companion to
/// [`StreamFn`] preserves source compatibility for existing callers while
/// allowing rich agent loops to forward the upstream `SimpleStreamOptions`.
pub type StreamFnWithOptions = Arc<
    dyn Fn(
            &pi_ai::model::Model,
            &Context,
            &pi_ai::types::SimpleStreamOptions,
        ) -> AssistantMessageEventStream
        + Send
        + Sync,
>;
pub type StreamEventObserver = Arc<dyn Fn(&pi_ai::AssistantMessageEvent) + Send + Sync>;

#[derive(Clone)]
pub struct AgentContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<AgentTool>,
    /// Replace model-facing image blocks with the upstream placeholder while
    /// retaining the original transcript for UI/session rendering.
    pub block_images: bool,
}

impl AgentContext {
    pub fn new(system_prompt: Option<String>, tools: Vec<AgentTool>) -> Self {
        Self {
            system_prompt,
            messages: Vec::new(),
            tools,
            block_images: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart {
        message: AgentMessage,
    },
    MessageEnd {
        message: AgentMessage,
    },
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
}

pub struct AgentLoopConfig {
    pub model: pi_ai::model::Model,
    pub stream_fn: StreamFn,
    pub signal: Option<Arc<AtomicBool>>,
    /// Stop after each turn (like `--print`); defaults to auto (stop when no
    /// tool calls and no follow-ups).
    pub stop_after_turn: bool,
    /// Optional raw stream-event observer (used by RPC mode to stream
    /// message_update events like the upstream AgentSession).
    pub on_stream_event: Option<StreamEventObserver>,
}

/// Runs the agent loop over the given prompts, mutating `context.messages`.
/// Returns newly produced messages (prompts + assistant + tool results).
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: &mut dyn FnMut(AgentEvent),
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_messages: Vec<AgentMessage> = context
        .messages
        .iter()
        .cloned()
        .chain(prompts.clone())
        .collect();
    context.messages = current_messages.clone();

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        });
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        });
    }

    loop {
        // Stream an assistant response.
        let message = stream_assistant_response(&current_messages, context, config).await;
        let assistant_message = AgentMessage::Core(Message::Assistant(message.clone()));
        current_messages.push(assistant_message.clone());
        context.messages = current_messages.clone();
        new_messages.push(assistant_message.clone());
        emit(AgentEvent::MessageStart {
            message: assistant_message.clone(),
        });
        emit(AgentEvent::MessageEnd {
            message: assistant_message,
        });

        // Terminal error/abort ends the run.
        if matches!(
            message.stop_reason(),
            Some(StopReason::Error) | Some(StopReason::Aborted)
        ) {
            emit(AgentEvent::TurnEnd {
                message,
                tool_results: vec![],
            });
            let agent_end = AgentEvent::AgentEnd {
                messages: new_messages.clone(),
            };
            emit(agent_end);
            return new_messages;
        }

        // Execute tool calls (fail truncated batch when stop reason is length).
        let tool_calls: Vec<pi_ai::types::ContentBlock> = message
            .content()
            .iter()
            .filter(|c| matches!(c, pi_ai::types::ContentBlock::ToolCall { .. }))
            .cloned()
            .collect();
        let mut tool_results: Vec<ToolResultMessage> = Vec::new();
        let mut terminate_flags = Vec::new();
        if !tool_calls.is_empty() {
            let truncated = message.stop_reason() == Some(StopReason::Length);
            for call in &tool_calls {
                let result = match call {
                    pi_ai::types::ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => {
                        if truncated {
                            terminate_flags.push(false);
                            ToolResultMessage::text(
                                id.clone(),
                                name.clone(),
                                format!("Tool call was truncated by the token limit: {arguments}"),
                                true,
                            )
                        } else {
                            match context.tools.iter().find(|t| t.tool.name == *name) {
                                Some(tool) => {
                                    let (result, is_error) = execute_tool_with_cancellation(
                                        tool,
                                        id.clone(),
                                        arguments.clone(),
                                        config.signal.clone(),
                                    )
                                    .await;
                                    terminate_flags.push(result.terminate && !is_error);
                                    agent_tool_result_to_message(
                                        id.clone(),
                                        name.clone(),
                                        &result,
                                        is_error,
                                    )
                                }
                                None => {
                                    terminate_flags.push(false);
                                    ToolResultMessage::text(
                                        id.clone(),
                                        name.clone(),
                                        format!("Unknown tool: {name}"),
                                        true,
                                    )
                                }
                            }
                        }
                    }
                    _ => continue,
                };
                current_messages.push(AgentMessage::Core(Message::ToolResult(result.clone())));
                new_messages.push(AgentMessage::Core(Message::ToolResult(result.clone())));
                tool_results.push(result);
            }
        }

        let batch_terminates =
            !terminate_flags.is_empty() && terminate_flags.iter().all(|terminated| *terminated);
        let has_more_tool_calls = !tool_calls.is_empty() && !batch_terminates;

        emit(AgentEvent::TurnEnd {
            message: message.clone(),
            tool_results: tool_results.clone(),
        });

        if config.stop_after_turn || !has_more_tool_calls {
            let agent_end = AgentEvent::AgentEnd {
                messages: new_messages.clone(),
            };
            emit(agent_end);
            return new_messages;
        }
    }
}

fn agent_tool_result_to_message(
    tool_call_id: String,
    tool_name: String,
    result: &crate::tools::AgentToolResult,
    is_error: bool,
) -> ToolResultMessage {
    let added_tool_names =
        (!result.added_tool_names.is_empty()).then(|| result.added_tool_names.clone());
    ToolResultMessage::ToolResult {
        tool_call_id,
        tool_name,
        content: result.content.clone(),
        details: result.details.clone(),
        usage: result.usage.clone(),
        added_tool_names,
        is_error,
        timestamp: pi_ai::types::now_ms(),
    }
}

async fn execute_tool_with_cancellation(
    tool: &AgentTool,
    tool_call_id: String,
    arguments: serde_json::Value,
    signal: Option<Arc<AtomicBool>>,
) -> (crate::tools::AgentToolResult, bool) {
    if is_aborted(signal.as_ref()) {
        return (
            crate::tools::AgentToolResult::text("Operation aborted"),
            true,
        );
    }
    let abort_signal = signal.clone();
    let tool = tool.clone();
    let mut execution = Box::pin(async move {
        let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (tool.execute)(tool_call_id, arguments, signal.clone(), None)
        })) {
            Ok(future) => future,
            Err(_) => return Err("tool execution panicked".to_string()),
        };
        std::panic::AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .map_err(|_| "tool execution panicked".to_string())?
    });
    let result = if let Some(signal) = abort_signal {
        tokio::select! {
            result = &mut execution => result,
            _ = wait_for_abort(signal) => {
                tokio::spawn(async move {
                    let _ = execution.await;
                });
                Err("Operation aborted".to_string())
            },
        }
    } else {
        execution.await
    };
    match result {
        Ok(result) => (result, false),
        Err(error) => (crate::tools::AgentToolResult::text(error), true),
    }
}

async fn stream_assistant_response(
    current_messages: &[AgentMessage],
    context: &AgentContext,
    config: &AgentLoopConfig,
) -> AssistantMessage {
    if is_aborted(config.signal.as_ref()) {
        return aborted_assistant_message(&config.model);
    }
    // Message conversion goes through the harness converter (messages.rs
    // `convertToLlm`): custom agent messages are rendered into user messages
    // (bash executions, custom content, compaction/branch summaries) instead
    // of being dropped at the provider boundary.
    let mut llm_messages = crate::messages::convert_to_llm(current_messages);
    if context.block_images {
        llm_messages = crate::messages::filter_images_for_provider(llm_messages);
    }
    let llm_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
        tools: context.tools.iter().map(|t| t.tool.clone()).collect(),
    };
    let stream = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (config.stream_fn)(&config.model, &llm_context)
    })) {
        Ok(stream) => stream,
        Err(payload) => {
            return error_assistant_message(&config.model, panic_payload_message(payload));
        }
    };
    let collect = async {
        if let Some(observer) = &config.on_stream_event {
            stream.collect_with_observer(observer).await
        } else {
            stream.collect().await.1
        }
    };
    if let Some(signal) = config.signal.clone() {
        tokio::select! {
            message = collect => message,
            _ = wait_for_abort(signal) => aborted_assistant_message(&config.model),
        }
    } else {
        collect.await
    }
}

fn aborted_assistant_message(model: &pi_ai::model::Model) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_stop_reason(StopReason::Aborted);
    message
}

fn error_assistant_message(model: &pi_ai::model::Model, error: String) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message.set_stop_reason(StopReason::Error);
    message.set_error_message(error);
    message
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "provider stream panicked".to_string()
}

async fn wait_for_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// Helpers to build user prompts from strings.
pub fn user_text_prompt(text: impl Into<String>, timestamp: u64) -> AgentMessage {
    AgentMessage::Core(Message::User(pi_ai::types::UserContent::blocks(
        vec![pi_ai::types::ContentBlock::text(text)],
        timestamp,
    )))
}

pub fn user_content_text(user: &pi_ai::types::UserContent) -> String {
    match user.content() {
        UserContentBody::String(s) => s.clone(),
        UserContentBody::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                pi_ai::types::ContentBlock::Text { text, .. } => text.clone(),
                _ => String::new(),
            })
            .collect(),
    }
}

/// Check whether the current signal has been aborted.
pub fn is_aborted(signal: Option<&Arc<AtomicBool>>) -> bool {
    signal.map(|s| s.load(Ordering::SeqCst)).unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::CustomAgentMessage;
    use pi_ai::providers::{
        faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
        RegisterFauxProviderOptions,
    };
    use pi_ai::types::{ContentBlock, UserContent};

    fn bash_message() -> AgentMessage {
        AgentMessage::Custom(CustomAgentMessage::BashExecution {
            command: "ls".into(),
            output: "a\nb".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 3,
            exclude_from_context: None,
        })
    }

    fn compaction_summary_message() -> AgentMessage {
        AgentMessage::Custom(CustomAgentMessage::CompactionSummary {
            summary: "history summarized".into(),
            tokens_before: 42,
            timestamp: 4,
        })
    }

    #[test]
    fn convert_to_llm_renders_custom_messages_for_the_provider() {
        let messages = vec![
            user_text_prompt("hello", 1),
            bash_message(),
            compaction_summary_message(),
        ];
        let llm = crate::messages::convert_to_llm(&messages);
        let texts: Vec<String> = llm
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => Some(user_content_text(u)),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 3);
        assert!(texts[0].contains("hello"));
        assert!(texts[1].contains("Ran `ls`"));
        assert!(texts[2].contains("compacted into the following summary"));
        assert!(texts[2].contains("history summarized"));
    }

    #[test]
    fn bash_execution_excluded_from_context_is_suppressed() {
        let mut message = bash_message();
        if let AgentMessage::Custom(CustomAgentMessage::BashExecution {
            exclude_from_context,
            ..
        }) = &mut message
        {
            *exclude_from_context = Some(true);
        }
        let llm = crate::messages::convert_to_llm(&[message]);
        assert_eq!(llm.len(), 0);
    }

    #[test]
    fn block_images_replaces_consecutive_provider_blocks_only() {
        let message = AgentMessage::Core(Message::User(UserContent::blocks(
            vec![
                ContentBlock::text("before"),
                ContentBlock::image("one", "image/png"),
                ContentBlock::image("two", "image/png"),
                ContentBlock::text("after"),
            ],
            1,
        )));
        let filtered =
            crate::messages::filter_images_for_provider(crate::messages::convert_to_llm(&[
                message,
            ]));
        let Message::User(UserContent::RoleUser { content, .. }) = &filtered[0] else {
            panic!("expected user message");
        };
        let UserContentBody::Blocks(blocks) = content else {
            panic!("expected block content");
        };
        assert_eq!(
            blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["before", "Image reading is disabled.", "after"]
        );
        assert!(!blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })));
    }

    #[test]
    fn legacy_loop_updates_context_and_emits_assistant_lifecycle_events() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("reply")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let stream_fn: StreamFn =
                Arc::new(move |model, context| core.stream(model, context, None));
            let config = AgentLoopConfig {
                model,
                stream_fn,
                signal: None,
                stop_after_turn: false,
                on_stream_event: None,
            };
            let mut context = AgentContext::new(None, Vec::new());
            let mut events = Vec::new();
            let messages = run_agent_loop(
                vec![user_text_prompt("hello", 1)],
                &mut context,
                &config,
                &mut |event| events.push(event),
            )
            .await;

            assert_eq!(messages.len(), 2);
            assert_eq!(context.messages, messages);
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::MessageStart {
                    message: AgentMessage::Core(Message::Assistant(_))
                }
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Core(Message::Assistant(_))
                }
            )));
        });
    }
}
