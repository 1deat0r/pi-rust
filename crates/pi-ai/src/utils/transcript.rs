//! Transcript system-message replay — port of
//! `packages/ai/src/utils/transcript.ts` (at e4c75a732).
//!
//! The leading system message is the system prompt; later system
//! messages change it: `content` adds instructions, `sections`
//! replace or remove named prompt sections, and
//! `tools_added`/`tools_removed` change the tool set. Replaying every
//! system message in order yields the current prompt and tools. A
//! message with `replace` discards the replayed state first, so it is
//! a complete new baseline. Providers that accept system messages
//! mid-conversation send each one in place; other providers, and every
//! provider after a replacement, rebuild the leading system message
//! from the replayed state.

use std::collections::BTreeMap;

use crate::types::{SystemMessage, Tool, ToolReference};

/// One transcript entry for replay purposes. Only `System` entries are
/// read; every other role passes through untouched (agent transcripts
/// may carry custom roles).
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptMessage {
    System(SystemMessage),
    Other { role: String },
}

impl TranscriptMessage {
    pub fn role(&self) -> &str {
        match self {
            TranscriptMessage::System(_) => "system",
            TranscriptMessage::Other { role } => role.as_str(),
        }
    }

    pub fn as_system(&self) -> Option<&SystemMessage> {
        match self {
            TranscriptMessage::System(message) => Some(message),
            TranscriptMessage::Other { .. } => None,
        }
    }
}

/// Render a system message as a complete prompt: its content followed
/// by its sections (upstream `getSystemMessageText`).
pub fn system_message_text(message: &SystemMessage) -> String {
    let mut parts = vec![message.content.clone()];
    if let Some(sections) = &message.sections {
        for value in sections.values().flatten() {
            parts.push(value.clone());
        }
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a later system message for APIs that accept system messages
/// mid-conversation (upstream `renderSystemMessageUpdate`). Section
/// changes are framed by name; request-time framing only.
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    if !message.content.is_empty() {
        parts.push(message.content.clone());
    }
    if let Some(sections) = &message.sections {
        for (name, value) in sections {
            match value {
                None => parts.push(format!("Removed system prompt section \"{name}\".")),
                Some(value) => parts.push(format!(
                    "Updated system prompt section \"{name}\":\n\n{value}"
                )),
            }
        }
    }
    parts.join("\n\n")
}

/// Resolve the tools available after applying every transcript delta
/// in order (upstream `getCurrentTools`).
pub fn current_tools(messages: &[TranscriptMessage]) -> Vec<Tool> {
    let mut tools: BTreeMap<String, Tool> = BTreeMap::new();
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        if system.replace == Some(true) {
            tools.clear();
        }
        if let Some(removed) = &system.tools_removed {
            for tool in removed {
                tools.remove(&tool.name);
            }
        }
        if let Some(added) = &system.tools_added {
            for tool in added {
                tools.insert(tool.name.clone(), tool.clone());
            }
        }
    }
    tools.into_values().collect()
}

/// Replay every system message into one leading system message holding
/// the current prompt and tools (upstream `getCurrentSystemMessage`).
/// Later `content` appends, `sections` patch by name (`None` removes),
/// a `replace` message starts over. Returns `None` when the transcript
/// has no timestamp and no tools.
pub fn current_system_message(messages: &[TranscriptMessage]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut timestamp: Option<u64> = None;
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        if system.replace == Some(true) {
            content.clear();
            sections.clear();
        }
        timestamp = timestamp.or(Some(system.timestamp));
        if !system.content.is_empty() {
            content.push(system.content.clone());
        }
        if let Some(message_sections) = &system.sections {
            for (name, value) in message_sections {
                if value.is_none() {
                    sections.remove(name);
                } else {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: content.join("\n\n"),
        sections: if sections.is_empty() {
            None
        } else {
            Some(sections)
        },
        tools_added: if tools.is_empty() { None } else { Some(tools) },
        tools_removed: None,
        replace: None,
        timestamp: timestamp.unwrap_or(0),
    })
}

/// Render the current system prompt text after replay (upstream
/// `getCurrentSystemPrompt`).
pub fn current_system_prompt(messages: &[TranscriptMessage]) -> String {
    current_system_message(messages)
        .map(|message| system_message_text(&message))
        .unwrap_or_default()
}

/// A replayed transcript: the replayed system message leads and every
/// later system message is dropped (upstream `collapseSystemMessages`,
/// for APIs without mid-conversation system messages).
pub fn collapse_system_messages(messages: &[TranscriptMessage]) -> Vec<TranscriptMessage> {
    let mut collapsed = Vec::new();
    if let Some(head) = current_system_message(messages) {
        collapsed.push(TranscriptMessage::System(head));
    }
    for message in messages {
        if message.role() != "system" {
            collapsed.push(message.clone());
        }
    }
    collapsed
}

/// Keep later system messages in place when the model accepts them;
/// otherwise collapse them. A replacement after the leading message
/// always collapses: no provider can retract the prompt it already
/// received (upstream `resolveTranscript`).
pub fn resolve_transcript(
    messages: &[TranscriptMessage],
    supports_mid_convo_system_messages: bool,
) -> Vec<TranscriptMessage> {
    let late_replacement = messages.iter().enumerate().any(|(index, message)| {
        index > 0 && message.as_system().is_some_and(|m| m.replace == Some(true))
    });
    if supports_mid_convo_system_messages && !late_replacement {
        messages.to_vec()
    } else {
        collapse_system_messages(messages)
    }
}

/// Strip executable and display-only fields from a tool before
/// transcript comparison (upstream `toToolDeclaration`). The JSON
/// round-trip drops unset optionals so both sides serialize alike.
pub fn tool_declaration(tool: &Tool) -> Tool {
    serde_json::from_value(serde_json::to_value(tool).unwrap_or(serde_json::Value::Null))
        .unwrap_or_else(|_| tool.clone())
}

/// Whether two tools declare the same model-visible interface
/// (upstream `declarationsEqual`, via serialized declarations).
pub fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    serde_json::to_value(tool_declaration(left)).ok()
        == serde_json::to_value(tool_declaration(right)).ok()
}

/// Added/removed tool state between two complete tool lists. A changed
/// definition is a removal followed by an addition (upstream
/// `getToolStateChanges`).
pub fn tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let previous_by_name: BTreeMap<&str, &Tool> = previous
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    let current_by_name: BTreeMap<&str, &Tool> = current
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    let tools_added = current
        .iter()
        .filter(|tool| {
            previous_by_name
                .get(tool.name.as_str())
                .is_none_or(|previous| !declarations_equal(previous, tool))
        })
        .map(tool_declaration)
        .collect();
    let tools_removed = previous
        .iter()
        .filter(|tool| {
            current_by_name
                .get(tool.name.as_str())
                .is_none_or(|current| !declarations_equal(tool, current))
        })
        .map(|tool| ToolReference {
            name: tool.name.clone(),
        })
        .collect();
    ToolStateChanges {
        tools_added,
        tools_removed,
    }
}

/// Added/removed tool state between two tool lists.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStateChanges {
    pub tools_added: Vec<Tool>,
    pub tools_removed: Vec<ToolReference>,
}

/// Whether a tool name was declared twice with different definitions
/// (upstream `hasToolRedefinitions`). Transports that reference tools
/// by name cannot express that.
pub fn has_tool_redefinitions(messages: &[TranscriptMessage]) -> bool {
    let mut declared: BTreeMap<String, Tool> = BTreeMap::new();
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        for tool in system.tools_added.iter().flatten() {
            if let Some(previous) = declared.get(&tool.name) {
                if !declarations_equal(previous, tool) {
                    return true;
                }
            }
            declared.insert(tool.name.clone(), tool.clone());
        }
    }
    false
}

/// Whether tool history contains a removal or same-name redeclaration
/// that an addition-only transport cannot replay (upstream
/// `hasNonAdditiveToolChanges`).
pub fn has_non_additive_tool_changes(messages: &[TranscriptMessage]) -> bool {
    let mut declared = std::collections::BTreeSet::new();
    for message in messages {
        let Some(system) = message.as_system() else {
            continue;
        };
        if system.tools_removed.as_ref().is_some_and(|r| !r.is_empty()) {
            return true;
        }
        for tool in system.tools_added.iter().flatten() {
            if !declared.insert(tool.name.clone()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::Tool;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: json!({"type": "object"}),
            constrained_sampling: None,
        }
    }

    fn system(
        content: &str,
        sections: &[(&str, Option<&str>)],
        added: &[&str],
        removed: &[&str],
        replace: bool,
        timestamp: u64,
    ) -> TranscriptMessage {
        TranscriptMessage::System(SystemMessage {
            content: content.to_string(),
            sections: if sections.is_empty() {
                None
            } else {
                Some(
                    sections
                        .iter()
                        .map(|(name, value)| ((*name).to_string(), value.map(|v| v.to_string())))
                        .collect(),
                )
            },
            tools_added: if added.is_empty() {
                None
            } else {
                Some(added.iter().map(|name| tool(name)).collect())
            },
            tools_removed: if removed.is_empty() {
                None
            } else {
                Some(
                    removed
                        .iter()
                        .map(|name| ToolReference {
                            name: (*name).to_string(),
                        })
                        .collect(),
                )
            },
            replace: replace.then_some(true),
            timestamp,
        })
    }

    fn user() -> TranscriptMessage {
        TranscriptMessage::Other {
            role: "user".to_string(),
        }
    }

    fn assistant() -> TranscriptMessage {
        TranscriptMessage::Other {
            role: "assistant".to_string(),
        }
    }

    fn transcript() -> Vec<TranscriptMessage> {
        vec![
            system(
                "base",
                &[("a", Some("<a>1</a>")), ("b", Some("<b>1</b>"))],
                &["first"],
                &[],
                false,
                10,
            ),
            user(),
            system("also do this", &[], &[], &[], false, 12),
            assistant(),
            system(
                "",
                &[
                    ("a", Some("<a>2</a>")),
                    ("b", None),
                    ("c", Some("<c>1</c>")),
                ],
                &["second"],
                &["first"],
                false,
                14,
            ),
        ]
    }

    #[test]
    fn replays_content_sections_and_tools_into_one_leading_message() {
        let messages = transcript();
        let current = current_system_message(&messages).expect("replayed head");
        assert_eq!(current.content, "base\n\nalso do this");
        assert_eq!(
            current.sections,
            Some(BTreeMap::from([
                ("a".to_string(), Some("<a>2</a>".to_string())),
                ("c".to_string(), Some("<c>1</c>".to_string())),
            ]))
        );
        assert_eq!(current.tools_added, Some(vec![tool("second")]));
        assert_eq!(current.timestamp, 10);
        assert_eq!(
            current_system_prompt(&messages),
            "base\n\nalso do this\n\n<a>2</a>\n\n<c>1</c>"
        );
    }

    #[test]
    fn collapse_keeps_only_non_system_messages_after_the_replayed_head() {
        let messages = transcript();
        let collapsed = collapse_system_messages(&messages);
        assert_eq!(
            collapsed.iter().map(|m| m.role()).collect::<Vec<_>>(),
            ["system", "user", "assistant"]
        );
        assert_eq!(collapse_system_messages(&collapsed), collapsed);
    }

    #[test]
    fn replacement_discards_replayed_state_and_always_collapses() {
        let mut messages = transcript();
        messages.push(system("forced", &[], &["third"], &[], true, 15));
        messages.push(system("", &[("d", Some("<d>1</d>"))], &[], &[], false, 16));
        let current = current_system_message(&messages).expect("replayed head");
        assert_eq!(current.content, "forced");
        assert_eq!(
            current.sections,
            Some(BTreeMap::from([(
                "d".to_string(),
                Some("<d>1</d>".to_string())
            )]))
        );
        assert_eq!(current.tools_added, Some(vec![tool("third")]));
        assert_eq!(current.timestamp, 10);
        assert_eq!(
            resolve_transcript(&messages, true),
            collapse_system_messages(&messages)
        );
        assert_eq!(resolve_transcript(&transcript(), true), transcript());
        let leading = vec![
            system("forced", &[], &["third"], &[], true, 15),
            system("", &[("d", Some("<d>1</d>"))], &[], &[], false, 16),
        ];
        assert_eq!(resolve_transcript(&leading, true), leading);
    }

    #[test]
    fn replay_without_system_messages_is_empty() {
        let messages = vec![user()];
        assert!(current_system_message(&messages).is_none());
        assert_eq!(current_system_prompt(&messages), "");
        assert_eq!(collapse_system_messages(&messages), messages);
    }

    #[test]
    fn renders_complete_prompts_and_framed_updates() {
        let messages = transcript();
        let (leading, update) = match (&messages[0], &messages[4]) {
            (TranscriptMessage::System(leading), TranscriptMessage::System(update)) => {
                (leading, update)
            }
            _ => panic!("expected system messages"),
        };
        assert_eq!(system_message_text(leading), "base\n\n<a>1</a>\n\n<b>1</b>");
        assert_eq!(
            render_system_message_update(update),
            [
                "Updated system prompt section \"a\":\n\n<a>2</a>",
                "Removed system prompt section \"b\".",
                "Updated system prompt section \"c\":\n\n<c>1</c>",
            ]
            .join("\n\n")
        );
    }
}
