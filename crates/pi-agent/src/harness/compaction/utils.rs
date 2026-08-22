//! Compaction utilities — port of
//! `packages/agent/src/harness/compaction/utils.ts` (file-operation
//! extraction/formatting and conversation serialization for summaries).

use std::collections::BTreeSet;

use pi_ai::types::{ContentBlock, JsonValue, Message, UserContentBody};

use crate::types::AgentMessage;

const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// File paths touched by a session branch or compaction range.
/// BTreeSet (insertion order in the JS source is irrelevant: `computeFileLists`
/// sorts before emitting) keeps behaviour deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOperations {
    /// Files read but not necessarily modified.
    pub read: BTreeSet<String>,
    /// Files written by full-file write operations.
    pub written: BTreeSet<String>,
    /// Files modified by edit operations.
    pub edited: BTreeSet<String>,
}

/// Create an empty file-operation accumulator.
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

/// Add file operations from assistant tool calls to an accumulator.
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    if message.role() != "assistant" {
        return;
    }
    let AgentMessage::Core(Message::Assistant(assistant)) = message else {
        return;
    };
    for block in assistant.content() {
        let ContentBlock::ToolCall { name, arguments, .. } = block else {
            continue;
        };
        let Some(path) = arguments.get("path").and_then(JsonValue::as_str) else {
            continue;
        };
        match name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

/// Compute sorted read-only and modified file lists from accumulated
/// operations.
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<&String> = file_ops.edited.iter().chain(file_ops.written.iter()).collect();
    let read_only: Vec<String> = file_ops.read.iter().filter(|f| !modified.contains(f)).cloned().collect();
    let modified_files: Vec<String> = modified.into_iter().cloned().collect();
    (read_only, modified_files)
}

/// Format file lists as summary metadata tags.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read_files.join("\n")));
    }
    if !modified_files.is_empty() {
        sections.push(format!("<modified-files>\n{}\n</modified-files>", modified_files.join("\n")));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

fn safe_json_stringify(value: &JsonValue) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let truncated_chars = text.len() - max_chars;
    format!("{}\n\n[... {truncated_chars} more characters truncated]", &text[..max_chars])
}

/// `contentText(content, fallback)` — concatenated text from user/toolResult
/// content (string or text blocks).
fn content_text(user_like: &UserContentBody, fallback: &str) -> String {
    match user_like {
        UserContentBody::String(s) => s.clone(),
        UserContentBody::Blocks(blocks) => {
            let text: String = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            if text.is_empty() {
                fallback.to_string()
            } else {
                text
            }
        }
    }
}

/// `contentText(content)` for assistant/toolResult block arrays (no fallback).
fn content_text_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Serialize LLM messages to plain text for summarization prompts.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => {
                let content = content_text(user.content(), "");
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();
                let mut has_text = false;

                for block in assistant.content() {
                    match block {
                        ContentBlock::Thinking { thinking, .. } => thinking_parts.push(thinking.clone()),
                        ContentBlock::ToolCall { name, arguments, .. } => {
                            let args_str = match arguments {
                                JsonValue::Object(map) => map
                                    .iter()
                                    .map(|(k, v)| format!("{k}={}", safe_json_stringify(v)))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                _ => safe_json_stringify(arguments),
                            };
                            tool_calls.push(format!("{name}({args_str})"));
                        }
                        ContentBlock::Text { .. } => has_text = true,
                        _ => {}
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking_parts.join("\n")));
                }
                if has_text {
                    parts.push(format!("[Assistant]: {}", content_text_blocks(assistant.content())));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let content = content_text_blocks(result.content());
                if !content.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)));
                }
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::{ContentBlock, UserContent};
    use crate::messages::convert_to_llm;
    use crate::types::CustomAgentMessage;
    use pi_ai::providers::faux_assistant_message;

    fn user(text: &str, ts: u64) -> Message {
        Message::User(UserContent::string(text, ts))
    }

    fn assistant_with_blocks(blocks: Vec<ContentBlock>, ts: u64) -> Message {
        Message::Assistant(faux_assistant_message(blocks, pi_ai::providers::FauxAssistantOptions::default()).with_timestamp(ts))
    }

    fn tool_result(text: &str, ts: u64) -> Message {
        Message::ToolResult(pi_ai::types::ToolResultMessage::text("tc1", "read", text, false).with_details_usage_timestamp(None, None, ts))
    }

    #[test]
    fn extract_ops_collects_read_write_edit_by_tool_name() {
        let mut ops = create_file_ops();
        let msg = AgentMessage::Core(assistant_with_blocks(
            vec![
                ContentBlock::tool_call("c1", "read", serde_json::json!({"path": "a.txt"})),
                ContentBlock::tool_call("c2", "write", serde_json::json!({"path": "b.txt"})),
                ContentBlock::tool_call("c3", "edit", serde_json::json!({"path": "c.txt"})),
                ContentBlock::tool_call("c4", "read", serde_json::json!({"path": "c.txt"})),
            ],
            1,
        ));
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.read.contains("a.txt"));
        assert!(ops.read.contains("c.txt"));
        assert!(ops.written.contains("b.txt"));
        assert!(ops.edited.contains("c.txt"));
    }

    #[test]
    fn extract_ops_ignores_non_assistant_and_missing_path() {
        let mut ops = create_file_ops();
        extract_file_ops_from_message(&AgentMessage::Core(user("hi", 1)), &mut ops);
        assert!(ops.read.is_empty());
        let msg = AgentMessage::Core(assistant_with_blocks(
            vec![ContentBlock::tool_call("c1", "read", serde_json::json!({"file": "x"}))],
            2,
        ));
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.read.is_empty());
    }

    #[test]
    fn compute_lists_dedupes_modified_from_read() {
        let mut ops = create_file_ops();
        ops.read.insert("a".into());
        ops.read.insert("b".into());
        ops.edited.insert("b".into());
        ops.written.insert("c".into());
        let (read, modified) = compute_file_lists(&ops);
        assert_eq!(read, vec!["a"]);
        assert_eq!(modified, vec!["b", "c"]);
    }

    #[test]
    fn format_ops_produces_upstream_tags() {
        let formatted = format_file_operations(&["a".into(), "b".into()], &["c".into()]);
        assert_eq!(
            formatted,
            "\n\n<read-files>\na\nb\n</read-files>\n\n<modified-files>\nc\n</modified-files>"
        );
        assert_eq!(format_file_operations(&[], &[]), "");
    }

    #[test]
    fn serialize_conversation_round_trip_shapes() {
        let messages: Vec<Message> = vec![
            user("Hello", 1),
            assistant_with_blocks(
                vec![
                    ContentBlock::thinking("think step"),
                    ContentBlock::text("Hi there"),
                    ContentBlock::tool_call("c1", "read", serde_json::json!({"path": "a.txt", "n": 1})),
                ],
                2,
            ),
            tool_result("file contents", 3),
        ];
        let text = serialize_conversation(&messages);
        assert!(text.contains("[User]: Hello"));
        assert!(text.contains("[Assistant thinking]: think step"));
        assert!(text.contains("[Assistant]: Hi there"));
        assert!(text.contains("[Assistant tool calls]: read(path=\"a.txt\", n=1)"));
        assert!(text.contains("[Tool result]: file contents"));
    }

    #[test]
    fn serialize_conversation_truncates_long_tool_results() {
        let long = "x".repeat(2500);
        let messages = vec![tool_result(&long, 1)];
        let text = serialize_conversation(&messages);
        assert!(text.contains("[... 500 more characters truncated]"));
    }

    #[test]
    fn convert_to_llm_then_serialize_includes_custom_messages() {
        // bash execution message converts to a user message
        let bash = AgentMessage::Custom(CustomAgentMessage::BashExecution {
            command: "ls".into(),
            output: "a\nb".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 3,
            exclude_from_context: None,
        });
        let llm = convert_to_llm(&[bash]);
        let text = serialize_conversation(&llm);
        assert!(text.contains("Ran `ls`"));
        assert!(text.contains("a\nb"));
    }

    #[test]
    fn serialize_uses_custom_content_message_for_custom() {
        let custom = AgentMessage::Custom(CustomAgentMessage::Custom {
            custom_type: "foo".into(),
            content: crate::types::CustomContent::String("custom text".into()),
            display: true,
            details: None,
            hook_type: None,
            timestamp: 5,
        });
        let llm = convert_to_llm(&[custom]);
        let text = serialize_conversation(&llm);
        assert!(text.contains("[User]: custom text"));
    }
}
