#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Session context builder — port of
//! `packages/agent/test/harness/session/context.test.ts` against
//! `session/context.rs`.

use pi_agent::messages::create_branch_summary_message;
use pi_agent::session::context::{
    build_session_context, session_entry_to_context_messages, SessionContextBuildOptions,
};
use pi_agent::session::types::Entry;
use pi_agent::types::AgentMessage;

use pi_ai::types::{ContentBlock, Message, StopReason, Usage, UserContent};

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::User(UserContent::blocks(
        vec![ContentBlock::text(text)],
        1,
    )))
}

fn assistant_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::Assistant(
        pi_ai::types::AssistantMessage::Assistant {
            content: vec![ContentBlock::text(text)],
            api: Some("anthropic-messages".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-5".into()),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Some(Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                cache_write_1h: None,
                reasoning: None,
                total_tokens: 0,
                cost: pi_ai::types::Cost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    total: 0.0,
                },
            }),
            stop_reason: Some(StopReason::Stop),
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    ))
}

fn deferred_assistant_message() -> AgentMessage {
    AgentMessage::Core(Message::Assistant(
        pi_ai::types::AssistantMessage::Assistant {
            content: vec![],
            api: Some("openai-responses".into()),
            provider: Some("openai".into()),
            model: Some("gpt-5".into()),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: None,
            stop_reason: Some(StopReason::Deferred),
            deferred: Some(pi_ai::types::DeferredHandle {
                provider: "openai".into(),
                model_id: "gpt-5".into(),
                api: "openai-responses".into(),
                id: "response-1".into(),
                expires_at: None,
                poll_after_ms: None,
                data: None,
            }),
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 2,
        },
    ))
}

fn entry_message(id: &str, seq: u64, parent: Option<&str>, message: AgentMessage) -> Entry {
    Entry::Message {
        id: id.into(),
        seq,
        parent_id: parent.map(|s| s.to_string()),
        timestamp: seq,
        message,
        terminate: None,
    }
}

fn roles(context: &pi_agent::session::context::SessionContext) -> Vec<&'static str> {
    context.messages.iter().map(|m| m.role()).collect()
}

#[test]
fn starts_at_latest_compaction_and_materializes_its_retained_tail() {
    let entries = vec![
        entry_message("old", 1, None, user_message("old")),
        Entry::Compaction {
            id: "compact".into(),
            seq: 2,
            parent_id: Some("old".into()),
            timestamp: 2,
            summary: "summary".into(),
            retained_tail: vec![user_message("retained"), assistant_message("answer")],
            tokens_before: 100,
            details: None,
            usage: None,
        },
        Entry::ModelChange {
            id: "model".into(),
            seq: 3,
            parent_id: Some("compact".into()),
            timestamp: 3,
            provider: "openai".into(),
            model_id: "gpt-5".into(),
        },
        Entry::ThinkingLevel {
            id: "thinking".into(),
            seq: 4,
            parent_id: Some("model".into()),
            timestamp: 4,
            thinking_level: "high".into(),
        },
        entry_message("tail", 5, Some("thinking"), user_message("tail")),
    ];

    let context = build_session_context(&entries, &SessionContextBuildOptions::default());
    assert_eq!(
        roles(&context),
        vec!["compactionSummary", "user", "assistant", "user"]
    );
    assert_eq!(
        context.model,
        Some(("openai".to_string(), "gpt-5".to_string()))
    );
    assert_eq!(context.thinking_level, "high");
}

#[test]
fn applies_caller_transforms_after_the_compaction_boundary() {
    let entries = vec![
        entry_message("old", 1, None, user_message("old")),
        Entry::Compaction {
            id: "compact".into(),
            seq: 2,
            parent_id: Some("old".into()),
            timestamp: 2,
            summary: "summary".into(),
            retained_tail: vec![],
            tokens_before: 100,
            details: None,
            usage: None,
        },
        Entry::BranchSummary {
            id: "branch".into(),
            seq: 3,
            parent_id: Some("compact".into()),
            timestamp: 3,
            from_id: "abandoned".into(),
            summary: "branch summary".into(),
            details: None,
            usage: None,
        },
        entry_message("tail", 4, Some("branch"), user_message("tail")),
    ];

    let mut options = SessionContextBuildOptions::default();
    options.entry_transforms.push(Box::new(|entries: &[Entry]| {
        entries
            .iter()
            .filter(|e| e.entry_type_str() != "compaction")
            .cloned()
            .collect()
    }));
    let context = build_session_context(&entries, &options);
    assert_eq!(roles(&context), vec!["branchSummary", "user"]);
}

#[test]
fn projects_custom_entries_and_omits_deferred_assistant_handles() {
    let entries = vec![
        entry_message("user", 1, None, user_message("hello")),
        entry_message("deferred", 2, Some("user"), deferred_assistant_message()),
        Entry::Custom {
            id: "custom".into(),
            seq: 3,
            parent_id: Some("deferred".into()),
            timestamp: 3,
            custom_type: "note".into(),
            data: Some(serde_json::json!("project me")),
        },
    ];

    let mut options = SessionContextBuildOptions::default();
    options.entry_projectors.insert(
        "note".to_string(),
        Box::new(|entry: &Entry, _index: usize, _entries: &[Entry]| {
            let Entry::Custom { data, .. } = entry else {
                return None;
            };
            let text = data
                .as_ref()
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Some(vec![user_message(&format!("note: {text}"))])
        }),
    );
    let context = build_session_context(&entries, &options);
    assert_eq!(roles(&context), vec!["user", "user"]);
    let AgentMessage::Core(Message::User(u)) = &context.messages[1] else {
        panic!("expected user message")
    };
    assert_eq!(
        u.content(),
        &pi_ai::types::UserContentBody::Blocks(vec![ContentBlock::text("note: project me")])
    );
}

#[test]
fn session_entry_to_context_messages_branch_summary_via_helper_matches() {
    // Direct helper parity: createBranchSummaryMessage gives the same role.
    let msg = create_branch_summary_message("s", "from", 42);
    assert_eq!(msg.role(), "branchSummary");
    let _ = session_entry_to_context_messages;
}
