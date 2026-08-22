//! GitHub Copilot request header helpers — port of
//! `packages/ai/src/api/github-copilot-headers.ts`.
//!
//! Copilot's proxy endpoint expects `X-Initiator` to say whether a request is
//! user- or agent-initiated (e.g. a follow-up after assistant/tool messages),
//! `Openai-Intent` for conversation-edits, and `Copilot-Vision-Request` when
//! the context contains images. The static Copilot model headers (User-Agent,
//! Editor-Version, ...) ride on `Model.headers`; these functions produce the
//! dynamic, context-dependent subset.

use crate::types::{ContentBlock, Message, UserContent, UserContentBody};

/// Copilot expects X-Initiator to indicate whether the request is
/// user-initiated or agent-initiated (e.g. follow-up after
/// assistant/tool messages).
pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        Some(m) if !matches!(m, Message::User(_)) => "agent",
        _ => "user",
    }
}

fn content_has_image(content: &UserContentBody) -> bool {
    match content {
        UserContentBody::Blocks(blocks) => blocks.iter().any(|b| matches!(b, ContentBlock::Image { .. })),
        UserContentBody::String(_) => false,
    }
}

/// Copilot requires `Copilot-Vision-Request: true` when images are present.
pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|msg| match msg {
        Message::User(UserContent::RoleUser { content, .. }) => content_has_image(content),
        Message::ToolResult(t) => t.content().iter().any(|b| matches!(b, ContentBlock::Image { .. })),
        _ => false,
    })
}

/// Build the dynamic headers for a request to the Copilot proxy.
/// `has_images` is precomputed by the caller (mirrors upstream, where the
/// vision-scan runs once for both `stream` and `streamSimple`).
pub fn build_copilot_dynamic_headers(messages: &[Message], has_images: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("X-Initiator".to_string(), infer_copilot_initiator(messages).to_string()),
        ("Openai-Intent".to_string(), "conversation-edits".to_string()),
    ];
    if has_images {
        headers.push(("Copilot-Vision-Request".to_string(), "true".to_string()));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, ToolResultMessage};

    fn user_msg(s: &str) -> Message {
        Message::User(UserContent::string(s, 1))
    }

    fn assistant_msg() -> Message {
        Message::Assistant(AssistantMessage::new())
    }

    #[test]
    fn empty_messages_are_user_initiated() {
        assert_eq!(infer_copilot_initiator(&[]), "user");
    }

    #[test]
    fn user_last_implies_user_initiator() {
        let messages = vec![assistant_msg(), user_msg("Hello")];
        assert_eq!(infer_copilot_initiator(&messages), "user");
    }

    #[test]
    fn non_user_last_implies_agent_initiator() {
        let messages = vec![user_msg("Hello"), assistant_msg()];
        assert_eq!(infer_copilot_initiator(&messages), "agent");

        let tool_result = Message::ToolResult(ToolResultMessage::text("t1", "read", "ok", false));
        assert_eq!(infer_copilot_initiator(&[user_msg("x"), tool_result]), "agent");
    }

    #[test]
    fn vision_detected_in_user_image_blocks() {
        let blocks = vec![
            ContentBlock::text("what is this"),
            ContentBlock::image("aGVsbG8=", "image/png"),
        ];
        let messages = vec![Message::User(UserContent::blocks(blocks, 2))];
        assert!(has_copilot_vision_input(&messages));
    }

    #[test]
    fn vision_detected_in_tool_result_images() {
        let messages = vec![Message::ToolResult(ToolResultMessage::new(
            "t1",
            "read",
            vec![ContentBlock::image("aGVsbG8=", "image/jpeg")],
            false,
        ))];
        assert!(has_copilot_vision_input(&messages));
    }

    #[test]
    fn no_vision_without_images() {
        assert!(!has_copilot_vision_input(&[user_msg("plain text")]));
    }

    #[test]
    fn builds_static_headers_plus_vision_flag() {
        let headers = build_copilot_dynamic_headers(&[user_msg("hi")], true);
        let map: std::collections::BTreeMap<_, _> = headers.iter().cloned().collect();
        assert_eq!(map.get("X-Initiator").map(|s| s.as_str()), Some("user"));
        assert_eq!(map.get("Openai-Intent").map(|s| s.as_str()), Some("conversation-edits"));
        assert_eq!(map.get("Copilot-Vision-Request").map(|s| s.as_str()), Some("true"));
        assert_eq!(headers.len(), 3);
    }

    #[test]
    fn omits_vision_flag_when_no_images() {
        let headers = build_copilot_dynamic_headers(&[assistant_msg()], false);
        assert_eq!(headers.len(), 2);
        assert!(!headers.iter().any(|(k, _)| k == "Copilot-Vision-Request"));
    }
}
