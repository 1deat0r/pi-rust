//! Context token estimation — port of `packages/ai/src/utils/estimate.ts`.
//! Used by thinking budget clamping in the Bedrock adaptor (upstream
//! `clampMaxTokensToContext`).

use serde_json::Value;

use crate::types::{ContentBlock, Context, Message, Tool, Usage, UserContentBody};

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
    let tokens = if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    };
    // Context estimates are consumed by non-negative window arithmetic. A
    // negative usage adjustment is a ledger correction, not usable context.
    tokens.max(0) as u64
}

fn safe_json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

fn utf16_code_units(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn estimate_text_and_image_content_chars(content: &UserContentBody) -> u64 {
    match content {
        UserContentBody::String(s) => utf16_code_units(s),
        UserContentBody::Blocks(blocks) => {
            let mut chars = 0u64;
            for block in blocks {
                match block {
                    ContentBlock::Text { text, .. } => chars += utf16_code_units(text),
                    ContentBlock::Image { .. } => chars += ESTIMATED_IMAGE_CHARS,
                    _ => {}
                }
            }
            chars
        }
    }
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    utf16_code_units(text).div_ceil(CHARS_PER_TOKEN)
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
                    ContentBlock::Text { text, .. } => chars += utf16_code_units(text),
                    _ => {}
                }
            }
            chars.div_ceil(CHARS_PER_TOKEN)
        }
        Message::Assistant(a) => {
            let mut chars = 0u64;
            for block in a.content() {
                match block {
                    ContentBlock::Text { text, .. } => chars += utf16_code_units(text),
                    ContentBlock::Thinking { thinking, .. } => chars += utf16_code_units(thinking),
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        chars += utf16_code_units(name)
                            + utf16_code_units(&safe_json_stringify(arguments));
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
                && !matches!(
                    a.stop_reason(),
                    Some(crate::types::StopReason::Aborted) | Some(crate::types::StopReason::Error)
                )
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
                added_tool_names: Some(names),
                ..
            }) = message
            {
                for name in names {
                    added_names.insert(name.clone());
                }
            }
        }
        let added_tool_tokens = estimate_tools_tokens(
            &context
                .tools
                .iter()
                .filter(|t| added_names.contains(&t.name))
                .cloned()
                .collect::<Vec<_>>(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, ContentBlock, Message, ToolResultMessage, UserContent};

    fn assistant_with_usage(timestamp: u64, total_tokens: i64) -> AssistantMessage {
        let mut assistant = AssistantMessage::new().with_timestamp(timestamp);
        assistant.set_api_provider_model("openai-responses", "openai", "test-model");
        assistant.set_stop_reason(crate::types::StopReason::Stop);
        assistant.set_usage(Usage {
            input: total_tokens,
            total_tokens,
            ..Default::default()
        });
        *assistant.content_mut() = vec![ContentBlock::text("kept")];
        assistant
    }

    #[test]
    fn estimates_text_tokens_as_chars_over_four() {
        assert_eq!(estimate_text_tokens("hello"), 2); // 5 chars -> ceil(5/4)
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("abcdefgh"), 2);
    }

    #[test]
    fn estimates_javascript_utf16_code_units_for_astral_unicode() {
        // JavaScript String.length counts each 😀 as two UTF-16 code units.
        // Three emoji therefore estimate as ceil(6 / 4) = 2 tokens.
        assert_eq!(estimate_text_tokens("😀😀😀"), 2);
        let user = Message::User(UserContent::string("😀😀😀", 1));
        assert_eq!(estimate_message_tokens(&user), 2);
    }

    #[test]
    fn estimates_message_tokens() {
        let user = Message::User(UserContent::string("Hello world this is a test", 1)); // 25 chars -> 7
        assert_eq!(estimate_message_tokens(&user), 7);
        let mut assistant = AssistantMessage::new();
        assistant.set_api_provider_model("anthropic-messages", "anthropic", "claude-sonnet-4-6");
        *assistant.content_mut() = vec![
            ContentBlock::text("four"),           // 4 chars -> 1
            ContentBlock::thinking("think more"), // 10 chars -> 3
        ];
        let assistant_msg = Message::Assistant(assistant);
        assert_eq!(estimate_message_tokens(&assistant_msg), 4);
    }

    #[test]
    fn estimates_images_at_4800_chars() {
        let user = Message::User(UserContent::blocks(
            vec![ContentBlock::image("aGVsbG8=", "image/png")],
            1,
        ));
        assert_eq!(estimate_message_tokens(&user), 1200); // 4800 / 4
    }

    #[test]
    fn context_estimate_includes_system_and_tools() {
        let context = Context {
            system_prompt: Some("you are helpful".to_string()), // 16 chars -> 4
            messages: vec![Message::User(UserContent::string("hi", 1))], // 2 chars -> 1
            tools: vec![crate::types::json_tool(
                "lookup",
                "look up",
                &serde_json::json!({ "type": "object" }),
            )],
        };
        let est = estimate_context_tokens(&context);
        assert!(est.tokens > 5, "got {}", est.tokens);
        assert_eq!(est.last_usage_index, None);
    }

    #[test]
    fn context_estimate_uses_last_assistant_usage_when_present() {
        let mut assistant = AssistantMessage::new();
        assistant.set_api_provider_model("anthropic-messages", "anthropic", "claude-sonnet-4-6");
        assistant.set_stop_reason(crate::types::StopReason::Stop);
        assistant.set_usage(Usage {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 150,
            cost: Default::default(),
        });
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::Assistant(assistant),
                Message::User(UserContent::string("follow up here", 2)), // 14 chars -> 4
            ],
            tools: vec![],
        };
        let est = estimate_context_tokens(&context);
        assert_eq!(est.usage_tokens, 150);
        assert_eq!(est.trailing_tokens, 4);
        assert_eq!(est.tokens, 154);
        assert_eq!(est.last_usage_index, Some(0));
    }

    #[test]
    fn ignores_stale_usage_after_a_newer_prefix_message() {
        let context = Context {
            system_prompt: Some("system".into()),
            messages: vec![
                Message::User(UserContent::string("summary", 200)),
                Message::Assistant(assistant_with_usage(100, 9_500)),
                Message::User(UserContent::string("x".repeat(4_000), 300)),
            ],
            tools: vec![],
        };

        assert_eq!(
            estimate_context_tokens(&context),
            ContextUsageEstimate {
                tokens: 1_005,
                usage_tokens: 0,
                trailing_tokens: 1_005,
                last_usage_index: None,
            }
        );
    }

    #[test]
    fn reuses_fresh_usage_after_an_inserted_prefix() {
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserContent::string("summary", 200)),
                Message::Assistant(assistant_with_usage(100, 9_500)),
                Message::User(UserContent::string("new prompt", 300)),
                Message::Assistant(assistant_with_usage(400, 2_000)),
                Message::User(UserContent::string("tail", 500)),
            ],
            tools: vec![],
        };

        assert_eq!(
            estimate_context_tokens(&context),
            ContextUsageEstimate {
                tokens: 2_001,
                usage_tokens: 2_000,
                trailing_tokens: 1,
                last_usage_index: Some(3),
            }
        );
    }

    #[test]
    fn counts_only_tool_definitions_added_after_the_usage_checkpoint() {
        let base = crate::types::json_tool(
            "base_tool",
            "base",
            &serde_json::json!({ "type": "object" }),
        );
        let late = crate::types::json_tool(
            "late_tool",
            &"x".repeat(4_000),
            &serde_json::json!({ "type": "object" }),
        );
        let result = Message::ToolResult(ToolResultMessage::ToolResult {
            tool_call_id: "call-1".into(),
            tool_name: "load_tool".into(),
            content: vec![ContentBlock::text("loaded")],
            details: None,
            usage: None,
            added_tool_names: Some(vec!["late_tool".into()]),
            is_error: false,
            timestamp: 2,
        });
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::Assistant(assistant_with_usage(1, 100)),
                result.clone(),
            ],
            tools: vec![base, late.clone()],
        };

        let estimate = estimate_context_tokens(&context);
        let late_tokens = estimate_tools_tokens(&[late]);
        let result_tokens = estimate_message_tokens(&result);
        assert_eq!(estimate.usage_tokens, 100);
        assert_eq!(estimate.trailing_tokens, result_tokens + late_tokens);
        assert_eq!(estimate.tokens, 100 + result_tokens + late_tokens);
    }

    #[test]
    fn zero_usage_falls_back_to_estimating_the_complete_context() {
        let context = Context {
            system_prompt: Some("sys".into()),
            messages: vec![
                Message::Assistant(assistant_with_usage(1, 0)),
                Message::User(UserContent::string("tail", 2)),
            ],
            tools: vec![],
        };
        let estimate = estimate_context_tokens(&context);
        assert_eq!(estimate.last_usage_index, None);
        assert_eq!(estimate.usage_tokens, 0);
        assert_eq!(estimate.tokens, 3);
        assert_eq!(estimate.trailing_tokens, 3);
    }
}
