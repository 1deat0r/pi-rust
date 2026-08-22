//! Context token estimation — port of `packages/ai/src/utils/estimate.ts`.
//! Used by thinking budget clamping in the Bedrock adaptor (upstream
//! `clampMaxTokensToContext`).

use serde_json::Value;

use crate::types::{
    ContentBlock, Context, Message, Tool, Usage, UserContentBody,
};

const CHARS_PER_TOKEN: u64 = 4;
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

pub trait ContextEstimate {
    fn tokens(&self) -> u64;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn safe_json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

fn estimate_text_and_image_content_chars(content: &UserContentBody) -> u64 {
    match content {
        UserContentBody::String(s) => s.chars().count() as u64,
        UserContentBody::Blocks(blocks) => {
            let mut chars = 0u64;
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => chars += text.chars().count() as u64,
                    ContentBlock::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                    _ => {}
                }
            }
            chars
        }
    }
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(CHARS_PER_TOKEN)
}

fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(u) => {
            (estimate_text_and_image_content_chars(u.content())).div_ceil(CHARS_PER_TOKEN)
        }
        Message::ToolResult(t) => {
            let mut chars = 0u64;
            for block in t.content() {
                match block {
                    ContentBlock::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                    ContentBlock::Text { text, .. } => chars += text.chars().count() as u64,
                    _ => {}
                }
            }
            chars.div_ceil(CHARS_PER_TOKEN)
        }
        Message::Assistant(a) => {
            let mut chars = 0u64;
            for block in a.content() {
                match block {
                    ContentBlock::Text { text, .. } => chars += text.chars().count() as u64,
                    ContentBlock::Thinking { thinking, .. } => chars += thinking.chars().count() as u64,
                    ContentBlock::ToolCall { name, arguments, .. } => {
                        chars += name.chars().count() as u64 + safe_json_stringify(arguments).chars().count() as u64;
                    }
                    _ => {}
                }
            }
            chars.div_ceil(CHARS_PER_TOKEN)
        }
    }
}

fn get_last_assistant_usage_info(messages: &[Message]) -> Option<(Usage, usize)> {
    let mut latest_prefix_timestamp = u64::MIN;
    let mut usage_info: Option<(Usage, usize)> = None;
    for (i, message) in messages.iter().enumerate() {
        if let Message::Assistant(a) = message {
            let usage_applies_to_prefix = a.timestamp() >= latest_prefix_timestamp;
            let mut has_usage = false;
            if let Some(u) = a.usage() {
                has_usage = calculate_context_tokens(u) > 0;
            }
            if usage_applies_to_prefix
                && !matches!(a.stop_reason(), Some(crate::types::StopReason::Aborted) | Some(crate::types::StopReason::Error))
                && has_usage
            {
                usage_info = Some((a.usage().cloned().unwrap_or_default(), i));
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(message_timestamp(message));
    }
    usage_info
}

fn message_timestamp(message: &Message) -> u64 {
    match message {
        Message::User(u) => u.timestamp(),
        Message::ToolResult(t) => t.timestamp(),
        Message::Assistant(a) => a.timestamp(),
    }
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    if let Some((usage, index)) = get_last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(&usage);
        let mut trailing_tokens = 0u64;
        for message in &messages[index + 1..] {
            trailing_tokens += estimate_message_tokens(message);
        }
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        };
    }
    let mut tokens = 0u64;
    for message in messages {
        tokens += estimate_message_tokens(message);
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens(tools: &[Tool]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    let value = serde_json::to_value(tools).unwrap_or(Value::Null);
    estimate_text_tokens(&safe_json_stringify(&value))
}

/// Estimate the token count of a context (upstream `estimateContextTokens`).
pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let estimate = estimate_messages(&context.messages);
    if let Some(last_usage_index) = estimate.last_usage_index {
        let mut added_names = std::collections::BTreeSet::new();
        for message in &context.messages[last_usage_index + 1..] {
            if let Message::ToolResult(crate::types::ToolResultMessage::ToolResult {
                added_tool_names, ..
            }) = message
            {
                if let Some(names) = added_tool_names {
                    for name in names {
                        added_names.insert(name.clone());
                    }
                }
            }
        }
        let added_tool_tokens = estimate_tools_tokens(
            &context.tools.iter().filter(|t| added_names.contains(&t.name)).cloned().collect::<Vec<_>>(),
        );
        return ContextUsageEstimate {
            tokens: estimate.tokens + added_tool_tokens,
            usage_tokens: estimate.usage_tokens,
            trailing_tokens: estimate.trailing_tokens + added_tool_tokens,
            last_usage_index: Some(last_usage_index),
        };
    }
    let prefix_tokens = context
        .system_prompt
        .as_ref()
        .map(|s| estimate_text_tokens(s))
        .unwrap_or(0)
        + estimate_tools_tokens(&context.tools);
    ContextUsageEstimate {
        tokens: estimate.tokens + prefix_tokens,
        usage_tokens: estimate.usage_tokens,
        trailing_tokens: estimate.trailing_tokens + prefix_tokens,
        last_usage_index: estimate.last_usage_index,
    }
}

