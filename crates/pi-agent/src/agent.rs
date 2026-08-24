//! Agent loop — port of `packages/agent/src/agent-loop.ts` (the core
//! runLoop shape: turn streaming, tool execution, steering/follow-up
//! handling, termination signs). The harness-level hooks (compaction,
//! extensions, memory) layer on at the coding-agent level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pi_ai::types::{
    AssistantMessage, Context, Message, StopReason, ToolResultMessage, UserContentBody,
};
use pi_ai::AssistantMessageEventStream;

use crate::tools::AgentTool;
use crate::types::AgentMessage;

/// Provider stream function: `(model, context) -> event stream`.
pub type StreamFn =
    Arc<dyn Fn(&pi_ai::model::Model, &Context) -> AssistantMessageEventStream + Send + Sync>;

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
    pub on_stream_event: Option<Arc<dyn Fn(&pi_ai::AssistantMessageEvent) + Send + Sync>>,
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
        new_messages.push(AgentMessage::Core(Message::Assistant(message.clone())));

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
        let mut has_more_tool_calls = false;
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
                            ToolResultMessage::text(
                                id.clone(),
                                name.clone(),
                                format!("Tool call was truncated by the token limit: {arguments}"),
                                true,
                            )
                        } else {
                            match context.tools.iter().find(|t| t.tool.name == *name) {
                                Some(tool) => {
                                    match (tool.execute)(id.clone(), arguments.clone(), None, None)
                                        .await
                                    {
                                        Ok(result) => {
                                            let content = result.content;
                                            let details = result.details;
                                            let usage = result.usage;
                                            let added_tool_names =
                                                if result.added_tool_names.is_empty() {
                                                    None
                                                } else {
                                                    Some(result.added_tool_names)
                                                };
                                            ToolResultMessage::ToolResult {
                                                tool_call_id: id.clone(),
                                                tool_name: name.clone(),
                                                content,
                                                details,
                                                usage,
                                                added_tool_names,
                                                is_error: false,
                                                timestamp: pi_ai::types::now_ms(),
                                            }
                                        }
                                        Err(e) => ToolResultMessage::text(
                                            id.clone(),
                                            name.clone(),
                                            e,
                                            true,
                                        ),
                                    }
                                }
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

async fn stream_assistant_response(
    current_messages: &[AgentMessage],
    context: &AgentContext,
    config: &AgentLoopConfig,
) -> AssistantMessage {
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
    let stream = (config.stream_fn)(&config.model, &llm_context);
    if let Some(observer) = &config.on_stream_event {
        stream.collect_with_observer(observer).await
    } else {
        stream.collect().await.1
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
mod tests {
    use super::*;
    use crate::types::CustomAgentMessage;
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
}
