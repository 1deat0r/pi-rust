//! Agent loop — port of `packages/agent/src/agent-loop.ts` (the core
//! runLoop shape: turn streaming, tool execution, steering/follow-up
//! handling, termination signs). The harness-level hooks (compaction,
//! extensions, memory) layer on at the coding-agent level.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_ai::types::{
    AssistantMessage, Context, Message, StopReason, ToolResultMessage, UserContentBody,
};
use pi_ai::AssistantMessageEventStream;

use crate::types::AgentMessage;

/// A callable tool: name/description/schema plus execution.
#[derive(Clone)]
pub struct AgentTool {
    pub tool: pi_ai::types::Tool,
    pub execute: Arc<dyn Fn(&serde_json::Value) -> Result<ToolResultMessage, String> + Send + Sync>,
}

pub struct AgentContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<AgentTool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart { message: AgentMessage },
    MessageEnd { message: AgentMessage },
    TurnEnd { message: AssistantMessage, tool_results: Vec<ToolResultMessage> },
    AgentEnd { messages: Vec<AgentMessage> },
}

pub struct AgentLoopConfig {
    pub model: pi_ai::model::Model,
    pub stream_fn: Arc<dyn Fn(&pi_ai::model::Model, &Context) -> AssistantMessageEventStream + Send + Sync>,
    pub signal: Option<Arc<AtomicBool>>,
    /// Stop after each turn (like `--print`); defaults to auto (stop when no
    /// tool calls and no follow-ups).
    pub stop_after_turn: bool,
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
    let mut current_messages: Vec<AgentMessage> =
        context.messages.iter().cloned().chain(prompts.clone()).collect();

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for prompt in &prompts {
        emit(AgentEvent::MessageStart { message: prompt.clone() });
        emit(AgentEvent::MessageEnd { message: prompt.clone() });
    }

    loop {
        // Stream an assistant response.
        let message =
            stream_assistant_response(&current_messages, context, config).await;
        new_messages.push(AgentMessage::Core(Message::Assistant(message.clone())));

        // Terminal error/abort ends the run.
        if matches!(message.stop_reason(), Some(StopReason::Error) | Some(StopReason::Aborted)) {
            emit(AgentEvent::TurnEnd { message, tool_results: vec![] });
            let agent_end = AgentEvent::AgentEnd { messages: new_messages.clone() };
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
        let mut has_more_tool_calls = false;
        if !tool_calls.is_empty() {
            let truncated = message.stop_reason() == Some(StopReason::Length);
            for call in &tool_calls {
                let result = match call {
                    pi_ai::types::ContentBlock::ToolCall { id, name, arguments, .. } => {
                        if truncated {
                            ToolResultMessage::text(
                                id.clone(),
                                name.clone(),
                                format!("Tool call was truncated by the token limit: {arguments}"),
                                true,
                            )
                        } else {
                            match context.tools.iter().find(|t| t.tool.name == *name) {
                                Some(tool) => (tool.execute)(arguments).unwrap_or_else(|e| {
                                    ToolResultMessage::text(id.clone(), name.clone(), e, true)
                                }),
                                None => ToolResultMessage::text(
                                    id.clone(),
                                    name.clone(),
                                    format!("Unknown tool: {name}"),
                                    true,
                                ),
                            }
                        }
                    }
                    _ => continue,
                };
                current_messages.push(AgentMessage::Core(Message::ToolResult(result.clone())));
                new_messages.push(AgentMessage::Core(Message::ToolResult(result.clone())));
                tool_results.push(result);
            }
            has_more_tool_calls = tool_results.iter().any(|r| !r.is_error());
        }

        emit(AgentEvent::TurnEnd { message: message.clone(), tool_results: tool_results.clone() });

        if config.stop_after_turn || !has_more_tool_calls {
            let agent_end = AgentEvent::AgentEnd { messages: new_messages.clone() };
            emit(agent_end);
            return new_messages;
        }
    }
}

async fn stream_assistant_response(
    current_messages: &[AgentMessage],
    context: &AgentContext,
    config: &AgentLoopConfig,
) -> AssistantMessage {
    let llm_context = Context {
        system_prompt: context.system_prompt.clone(),
        messages: current_messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Core(message) => Some(message.clone()),
                // Custom messages are dropped in the base conversion; harness
                // hooks convert them at the coding-agent level.
                AgentMessage::Custom(_) => None,
            })
            .collect(),
        tools: context.tools.iter().map(|t| t.tool.clone()).collect(),
    };
    let stream = (config.stream_fn)(&config.model, &llm_context);
    stream.collect().await.1
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
