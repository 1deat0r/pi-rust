//! Custom-agent message helpers — port of
//! `packages/agent/src/harness/messages.ts`.

use pi_ai::types::{ContentBlock, Message, UserContent};

use crate::types::{AgentMessage, CustomAgentMessage};

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// `bashExecutionToText` — renders a bash execution message as model text.
pub fn bash_execution_to_text(msg: &CustomAgentMessage) -> String {
    let CustomAgentMessage::BashExecution {
        command,
        output,
        exit_code,
        cancelled,
        truncated,
        full_output_path,
        ..
    } = msg
    else {
        return String::new();
    };
    let mut text = format!("Ran `{command}`\n");
    if !output.is_empty() {
        text.push_str(&format!("```\n{output}\n```"));
    } else {
        text.push_str("(no output)");
    }
    if *cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code {
        if *code != 0 {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
    }
    if *truncated {
        if let Some(path) = full_output_path {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
    }
    text
}

/// `createBranchSummaryMessage(summary, fromId, timestamp)`.
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::BranchSummary {
        summary: summary.into(),
        from_id: from_id.into(),
        timestamp,
    })
}

/// `createCompactionSummaryMessage(summary, tokensBefore, timestamp)`.
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: u64,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::CompactionSummary {
        summary: summary.into(),
        tokens_before,
        timestamp,
    })
}

/// `createCustomMessage(customType, content, display, details, timestamp)`.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: CustomContentRef<'_>,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::Custom {
        custom_type: custom_type.into(),
        content: match content {
            CustomContentRef::String(s) => crate::types::CustomContent::String(s.to_string()),
            CustomContentRef::Blocks(b) => crate::types::CustomContent::Blocks(b.to_vec()),
        },
        display,
        details,
        hook_type: None,
        timestamp,
    })
}

/// Borrowed content for `create_custom_message`.
pub enum CustomContentRef<'a> {
    String(&'a str),
    Blocks(&'a [ContentBlock]),
}

/// `convertToLlm` — maps agent messages to LLM `Message`s, dropping
/// non-convertible roles (and bash executions marked `excludeFromContext`).
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Core(message) => Some(message.clone()),
            AgentMessage::Custom(custom) => match custom {
                CustomAgentMessage::BashExecution { .. } => {
                    if custom.exclude_from_context().unwrap_or(false) {
                        return None;
                    }
                    let text = bash_execution_to_text(custom);
                    Some(Message::User(UserContent::blocks(
                        vec![ContentBlock::text(text)],
                        custom.timestamp(),
                    )))
                }
                CustomAgentMessage::Custom {
                    content, timestamp, ..
                } => {
                    let blocks = match content {
                        crate::types::CustomContent::String(s) => {
                            vec![ContentBlock::text(s.clone())]
                        }
                        crate::types::CustomContent::Blocks(blocks) => blocks.clone(),
                    };
                    Some(Message::User(UserContent::blocks(blocks, *timestamp)))
                }
                CustomAgentMessage::BranchSummary {
                    summary, timestamp, ..
                } => {
                    let text = format!(
                        "{}{}{}",
                        BRANCH_SUMMARY_PREFIX, summary, BRANCH_SUMMARY_SUFFIX
                    );
                    Some(Message::User(UserContent::blocks(
                        vec![ContentBlock::text(text)],
                        *timestamp,
                    )))
                }
                CustomAgentMessage::CompactionSummary {
                    summary, timestamp, ..
                } => {
                    let text = format!(
                        "{}{}{}",
                        COMPACTION_SUMMARY_PREFIX, summary, COMPACTION_SUMMARY_SUFFIX
                    );
                    Some(Message::User(UserContent::blocks(
                        vec![ContentBlock::text(text)],
                        *timestamp,
                    )))
                }
            },
        })
        .collect()
}

/// Apply coding-agent's `blockImages` provider-boundary filter. The stored
/// transcript remains unchanged; only user and tool-result messages sent to
/// the provider are rewritten.
pub fn filter_images_for_provider(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|message| match message {
            Message::User(UserContent::RoleUser { content, timestamp }) => {
                let content = match content {
                    pi_ai::types::UserContentBody::String(text) => {
                        pi_ai::types::UserContentBody::String(text)
                    }
                    pi_ai::types::UserContentBody::Blocks(blocks) => {
                        pi_ai::types::UserContentBody::Blocks(replace_image_blocks(blocks))
                    }
                };
                Message::User(UserContent::RoleUser { content, timestamp })
            }
            Message::ToolResult(mut result) => {
                let pi_ai::types::ToolResultMessage::ToolResult { content, .. } = &mut result;
                let blocks = std::mem::take(content);
                *content = replace_image_blocks(blocks);
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
}

fn replace_image_blocks(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut filtered = Vec::with_capacity(blocks.len());
    for block in blocks {
        if matches!(block, ContentBlock::Image { .. }) {
            let is_duplicate = filtered.last().is_some_and(|previous| {
                matches!(
                    previous,
                    ContentBlock::Text { text, .. } if text == "Image reading is disabled."
                )
            });
            if !is_duplicate {
                filtered.push(ContentBlock::text("Image reading is disabled."));
            }
        } else {
            filtered.push(block);
        }
    }
    filtered
}

impl CustomAgentMessage {
    /// `excludeFromContext` getter for bash execution messages.
    pub fn exclude_from_context(&self) -> Option<bool> {
        match self {
            CustomAgentMessage::BashExecution {
                exclude_from_context,
                ..
            } => *exclude_from_context,
            _ => None,
        }
    }
}
