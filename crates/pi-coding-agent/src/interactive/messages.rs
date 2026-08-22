//! Message rendering for the interactive transcript — a lean port of
//! `packages/coding-agent/src/modes/interactive/components/` (assistant-message,
//! user-message, tool-execution, bash-execution, compaction-summary,
//! branch-summary, custom-message).
//!
//! Documented divergence: upstream renders live streaming components; this
//! port re-renders the transcript from the message vector each frame (which
//! the differential renderer makes inexpensive).

use pi_agent::types::AgentMessage;
use pi_ai::types::{ContentBlock, Message};

use crate::core::settings::SettingsManager;

/// Render one agent message into the transcript text (markdown source).
pub fn render_message(message: &AgentMessage, hide_thinking: bool) -> Option<(String, String)> {
    // Returns (kind, text) where kind is "user" | "assistant" | "tool" | "banner".
    match message {
        AgentMessage::Core(Message::User(u)) => {
            let text = pi_agent::agent::user_content_text(u);
            if text.trim().is_empty() {
                return None;
            }
            Some(("user".to_string(), text))
        }
        AgentMessage::Core(Message::Assistant(a)) => {
            let stop = a.stop_reason();
            let mut parts: Vec<String> = Vec::new();
            for block in a.content() {
                match block {
                    ContentBlock::Text { text, .. } => {
                        if !text.trim().is_empty() {
                            parts.push(text.clone());
                        }
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        if hide_thinking {
                            parts.push("> *Thinking…*".to_string());
                        } else if !thinking.trim().is_empty() {
                            parts.push(format!("> {}", thinking.trim()));
                        }
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                return None;
            }
            let mut text = parts.join("\n");
            if let Some(stop) = stop {
                match stop.as_str() {
                    "length" => text.push_str("\n\n_Response was truncated before completion._"),
                    "aborted" => text.push_str("\n\n_(operation aborted)_"),
                    "error" => text.push_str("\n\n_(error during generation)_"),
                    _ => {}
                }
            }
            Some(("assistant".to_string(), text))
        }
        AgentMessage::Core(Message::ToolResult(result)) => {
            let name = result.tool_name().to_string();
            let mut text = String::new();
            for block in result.content() {
                if let ContentBlock::Text { text: tb, .. } = block {
                    text.push_str(tb);
                    text.push('\n');
                }
            }
            let status = if result.is_error() { "✗" } else { "✓" };
            Some(("tool".to_string(), format!("{status} **{name}**\n{}", text.trim())))
        }
        AgentMessage::Custom(custom) => {
            match custom {
                pi_agent::types::CustomAgentMessage::BashExecution { command, output, exit_code, cancelled, .. } => {
                    let status = if *cancelled {
                        "(cancelled)".to_string()
                    } else if let Some(code) = exit_code {
                        if *code != 0 { format!("(exit {code})") } else { String::new() }
                    } else {
                        String::new()
                    };
                    let text = format!("$ `{command}`\n\n{}\n\n{}", output.trim(), status);
                    Some(("tool".to_string(), text))
                }
                pi_agent::types::CustomAgentMessage::CompactionSummary { summary, tokens_before, .. } => {
                    let tokens = format_tokens(*tokens_before);
                    Some(("banner".to_string(), format!("**[compaction]** Compacted from {tokens} tokens\n\n{summary}")))
                }
                pi_agent::types::CustomAgentMessage::BranchSummary { summary, .. } => {
                    Some(("banner".to_string(), format!("**[branch]** Branch summary\n\n{summary}")))
                }
                pi_agent::types::CustomAgentMessage::Custom { custom_type, content, .. } => {
                    let text = match content {
                        pi_agent::types::CustomContent::String(s) => s.clone(),
                        pi_agent::types::CustomContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    Some(("banner".to_string(), format!("**[{custom_type}]**\n\n{}", text.trim())))
                }
            }
        }
    }
}

/// Build the full transcript markdown given the message list.
pub fn build_transcript(messages: &[AgentMessage], hide_thinking: bool) -> String {
    let mut out = String::new();
    for message in messages {
        if let Some((kind, text)) = render_message(message, hide_thinking) {
            match kind.as_str() {
                "user" => {
                    out.push_str("### You\n\n");
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
                "assistant" | "tool" | "banner" => {
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
                _ => {}
            }
        }
    }
    out
}

/// Format a token count like upstream `formatTokens`.
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round() as u64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
}

/// Get the thinking level for the active model from settings.
pub fn current_thinking_level(settings: &SettingsManager, _provider: &str, _model_id: &str) -> Option<String> {
    settings.get_default_thinking_level().map(|s| s.to_string())
}
