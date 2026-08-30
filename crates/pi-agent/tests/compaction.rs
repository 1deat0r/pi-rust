#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Harness compaction + branch-summarization — port of
//! `packages/agent/test/harness/compaction.test.ts` and
//! `branch-summarization.test.ts` (LLM paths driven by scripted
//! `SimpleModels` responses; pure helpers asserted in lib tests).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pi_agent::fs::MemoryFs;
use pi_agent::harness::compaction::branch_summarization::{
    collect_entries_for_branch_summary, generate_branch_summary, prepare_branch_entries,
    GenerateBranchSummaryOptions,
};
use pi_agent::harness::compaction::{
    compact, estimate_context_tokens, generate_summary, generate_summary_with_usage,
    prepare_compaction, should_compact, CompactionPreparation, CompactionSettings,
    DEFAULT_COMPACTION_SETTINGS,
};
use pi_agent::harness::SimpleModels;
use pi_agent::session::memory::InMemorySessionStorage;
use pi_agent::session::types::{Entry, SessionMetadata};
use pi_agent::session::Session;
use pi_agent::types::AgentMessage;

use pi_ai::model::Model;
use pi_ai::providers::{faux_assistant_message, FauxAssistantOptions};
use pi_ai::types::{
    ContentBlock, Cost, Message, SimpleStreamOptions, StopReason, Usage, UserContent,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn zero_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

fn mock_usage(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::User(UserContent::blocks(
        vec![ContentBlock::text(text)],
        1,
    )))
}

fn assistant_message(text: &str, usage: Option<Usage>) -> AgentMessage {
    AgentMessage::Core(Message::Assistant(
        pi_ai::types::AssistantMessage::Assistant {
            content: vec![ContentBlock::text(text)],
            api: Some("anthropic-messages".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-5".into()),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage,
            stop_reason: Some(StopReason::Stop),
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
        },
    ))
}

fn message_entry(message: AgentMessage, id: &str, parent_id: Option<&str>, seq: u64) -> Entry {
    Entry::Message {
        id: id.to_string(),
        seq,
        parent_id: parent_id.map(|s| s.to_string()),
        timestamp: seq,
        message,
        terminate: None,
    }
}

fn compaction_entry(
    summary: &str,
    id: &str,
    parent_id: Option<&str>,
    seq: u64,
    retained_tail: Vec<AgentMessage>,
    details: Option<serde_json::Value>,
) -> Entry {
    Entry::Compaction {
        id: id.to_string(),
        seq,
        parent_id: parent_id.map(|s| s.to_string()),
        timestamp: seq,
        summary: summary.to_string(),
        retained_tail,
        tokens_before: 1234,
        details,
        usage: None,
    }
}

type ScriptedCalls = Arc<Mutex<Vec<(String, SimpleStreamOptions)>>>;

fn scripted_models(
    responses: Vec<pi_ai::types::AssistantMessage>,
) -> (SimpleModels, ScriptedCalls) {
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let queue_c = queue.clone();
    let calls_c = calls.clone();
    let models = SimpleModels::new(move |_model, context, options| {
        let queue = queue_c.clone();
        let calls = calls_c.clone();
        let context_text: String = context
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => match u.content() {
                    pi_ai::types::UserContentBody::String(s) => Some(s.clone()),
                    pi_ai::types::UserContentBody::Blocks(b) => Some(
                        b.iter()
                            .filter_map(|bl| match bl {
                                ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                            .collect(),
                    ),
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let options = options.clone();
        Box::pin(async move {
            calls.lock().unwrap().push((context_text, options));
            queue.lock().unwrap().pop_front().unwrap_or_else(|| {
                faux_assistant_message(
                    vec![],
                    FauxAssistantOptions {
                        stop_reason: Some(StopReason::Error),
                        error_message: Some(
                            "No scripted completeSimple response queued".to_string(),
                        ),
                    },
                )
            })
        })
    });
    (models, calls)
}

fn faux_model(reasoning: bool, max_tokens: u64) -> Model {
    let mut m = Model::new("test-model", "Test Model", "faux", "faux-test");
    m.reasoning = reasoning;
    m.context_window = 200_000;
    m.max_tokens = max_tokens;
    m
}

// ---------------------------------------------------------------------------
// Ported cases
// ---------------------------------------------------------------------------

#[test]
fn calculates_total_context_tokens_from_usage() {
    let usage = mock_usage(1000, 500, 200, 100);
    // totalTokens is the sum for this constructor; component sum is used when
    // totalTokens is 0.
    assert_eq!(
        pi_agent::harness::compaction::calculate_context_tokens(&usage),
        1800
    );
}

#[test]
fn checks_compaction_threshold() {
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 10000,
        keep_recent_tokens: 20000,
    };
    assert!(should_compact(95_000, 100_000, &settings));
    assert!(!should_compact(89_000, 100_000, &settings));
    assert!(!should_compact(
        95_000,
        100_000,
        &CompactionSettings {
            enabled: false,
            ..settings
        }
    ));
}

#[test]
fn estimates_context_tokens_with_last_usage_index() {
    let usage = mock_usage(10, 5, 3, 2);
    let assistant = assistant_message("assistant", Some(usage));
    let messages = vec![
        assistant_message("tail", Some(mock_usage(0, 0, 0, 0))),
        user_message("continue"),
    ];
    let estimate = estimate_context_tokens(&messages);
    // No assistant with usable usage in the trimmed list -> no usage index.
    assert_eq!(estimate.last_usage_index, None);
    assert!(estimate.tokens > 0);

    let messages = vec![assistant.clone(), user_message("tail")];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 20);
    assert_eq!(estimate.last_usage_index, Some(0));
    assert!(estimate.trailing_tokens > 0);
    assert_eq!(estimate.tokens, 20 + estimate.trailing_tokens);
}

#[test]
fn builds_session_context_with_compaction_entry() {
    let u1 = message_entry(user_message("1"), "u1", None, 1);
    let a1 = message_entry(assistant_message("a", None), "a1", Some("u1"), 2);
    let u2 = message_entry(user_message("2"), "u2", Some("a1"), 3);
    let a2 = message_entry(assistant_message("b", None), "a2", Some("u2"), 4);
    let compaction = compaction_entry(
        "Summary of 1,a,2,b",
        "c1",
        Some("a2"),
        5,
        vec![user_message("2"), assistant_message("b", None)],
        None,
    );
    let u3 = message_entry(user_message("3"), "u3", Some("c1"), 6);
    let a3 = message_entry(assistant_message("c", None), "a3", Some("u3"), 7);
    let loaded = pi_agent::session::context::build_session_context(
        &[u1, a1, u2, a2, compaction, u3, a3],
        &pi_agent::session::context::SessionContextBuildOptions::default(),
    );
    let roles: Vec<String> = loaded
        .messages
        .iter()
        .map(|m| m.role().to_string())
        .collect();
    assert_eq!(
        roles,
        vec![
            "compactionSummary",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
}

#[test]
fn prepares_compaction_using_latest_summary_as_previous() {
    let u1 = message_entry(user_message("user msg 1"), "u1", None, 1);
    let a1 = message_entry(
        assistant_message("assistant msg 1", None),
        "a1",
        Some("u1"),
        2,
    );
    let u2 = message_entry(user_message("user msg 2"), "u2", Some("a1"), 3);
    let a2 = message_entry(
        assistant_message("assistant msg 2", Some(mock_usage(5000, 1000, 0, 0))),
        "a2",
        Some("u2"),
        4,
    );
    let compaction1 = compaction_entry("First summary", "c1", Some("a2"), 5, vec![], None);
    let u3 = message_entry(user_message("user msg 3"), "u3", Some("c1"), 6);
    let a3 = message_entry(
        assistant_message("assistant msg 3", Some(mock_usage(8000, 2000, 0, 0))),
        "a3",
        Some("u3"),
        7,
    );
    let path_entries = vec![u1, a1, u2, a2, compaction1, u3, a3];
    let preparation = prepare_compaction(&path_entries, &DEFAULT_COMPACTION_SETTINGS)
        .unwrap()
        .unwrap();
    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("First summary")
    );
    assert!(!preparation.retained_tail.is_empty());
    assert_eq!(
        preparation.tokens_before,
        estimate_context_tokens(
            &pi_agent::session::context::build_session_context(
                &path_entries,
                &pi_agent::session::context::SessionContextBuildOptions::default(),
            )
            .messages
        )
        .tokens
    );
}

#[test]
fn carries_previous_compaction_retained_tail_into_next_preparation() {
    let retained_user = user_message("retained user");
    let retained_assistant = assistant_message("retained assistant", None);
    let compaction = compaction_entry(
        "previous summary",
        "c1",
        None,
        1,
        vec![retained_user.clone(), retained_assistant.clone()],
        None,
    );
    let user = message_entry(user_message("new user"), "u1", Some("c1"), 2);
    let assistant = message_entry(
        assistant_message("new assistant", None),
        "a1",
        Some("u1"),
        3,
    );

    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 100,
        keep_recent_tokens: 1,
    };
    let preparation = prepare_compaction(&[compaction, user, assistant], &settings)
        .unwrap()
        .unwrap();
    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("previous summary")
    );
    let mut collected = preparation.messages_to_summarize.clone();
    collected.extend(preparation.turn_prefix_messages.clone());
    collected.extend(preparation.retained_tail.clone());
    assert_eq!(
        collected,
        vec![
            retained_user,
            retained_assistant,
            user_message("new user"),
            assistant_message("new assistant", None)
        ]
    );
}

#[test]
fn prepares_split_turn_compaction_with_prior_file_operation_details() {
    let u1 = message_entry(user_message("user msg 1"), "u1", None, 1);
    let a1_message = {
        let mut am = pi_ai::types::AssistantMessage::new();
        *am.content_mut() = vec![ContentBlock::tool_call(
            "tool-1",
            "write",
            serde_json::json!({"path": "written.ts"}),
        )];
        am.set_api_provider_model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
        am.set_usage(mock_usage(100, 200, 0, 0));
        am.set_stop_reason(StopReason::Stop);
        AgentMessage::Core(Message::Assistant(am))
    };
    let a1 = message_entry(a1_message, "a1", Some("u1"), 2);
    let compaction1 = compaction_entry(
        "First summary",
        "c1",
        Some("a1"),
        3,
        vec![],
        Some(
            serde_json::json!({"readFiles": ["old-read.ts"], "modifiedFiles": ["old-edit.ts", "written.ts"]}),
        ),
    );
    let u2 = message_entry(user_message("large turn but small"), "u2", Some("c1"), 4);
    let a2 = message_entry(
        assistant_message("large assistant message", None),
        "a2",
        Some("u2"),
        5,
    );
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 100,
        keep_recent_tokens: 1,
    };
    let preparation = prepare_compaction(&[u1, a1, compaction1, u2, a2], &settings)
        .unwrap()
        .unwrap();

    assert_eq!(
        preparation.previous_summary.as_deref(),
        Some("First summary")
    );
    assert!(preparation.is_split_turn);
    assert_eq!(preparation.turn_prefix_messages.len(), 1);
    assert_eq!(preparation.turn_prefix_messages[0].role(), "user");
    assert!(preparation.file_ops.read.contains("old-read.ts"));
    assert!(preparation.file_ops.edited.contains("old-edit.ts"));
    assert!(preparation.file_ops.edited.contains("written.ts"));
}

#[test]
fn does_not_prepare_compaction_when_nothing_valid() {
    let compaction = compaction_entry("already compacted", "c1", None, 1, vec![], None);
    assert!(
        prepare_compaction(&[compaction], &DEFAULT_COMPACTION_SETTINGS)
            .unwrap()
            .is_none()
    );
    assert!(prepare_compaction(&[], &DEFAULT_COMPACTION_SETTINGS)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn passes_reasoning_only_for_reasoning_models_with_thinking_enabled() {
    let messages = vec![user_message("Summarize this.")];
    let (models, calls) = scripted_models(vec![faux_assistant_message(
        vec![ContentBlock::text("## Goal\nTest summary")],
        FauxAssistantOptions::default(),
    )]);
    let model = faux_model(true, 8192);
    generate_summary(
        &messages,
        &models,
        &model,
        2000,
        None,
        None,
        None,
        Some("medium"),
        None,
        None,
    )
    .await
    .unwrap();
    let captured = calls.lock().unwrap().clone();
    assert_eq!(
        captured[0].1.reasoning,
        Some(pi_ai::types::ThinkingLevel::Medium)
    );

    let (models, calls) = scripted_models(vec![faux_assistant_message(
        vec![ContentBlock::text("## Goal\nTest summary")],
        FauxAssistantOptions::default(),
    )]);
    generate_summary(
        &messages,
        &models,
        &model,
        2000,
        None,
        None,
        None,
        Some("off"),
        None,
        None,
    )
    .await
    .unwrap();
    let captured = calls.lock().unwrap().clone();
    assert_eq!(captured[0].1.reasoning, None);

    let non_reasoning = faux_model(false, 8192);
    let (models, calls) = scripted_models(vec![faux_assistant_message(
        vec![ContentBlock::text("## Goal\nTest summary")],
        FauxAssistantOptions::default(),
    )]);
    generate_summary(
        &messages,
        &models,
        &non_reasoning,
        2000,
        None,
        None,
        None,
        Some("medium"),
        None,
        None,
    )
    .await
    .unwrap();
    let captured = calls.lock().unwrap().clone();
    assert_eq!(captured[0].1.reasoning, None);
}

#[tokio::test]
async fn includes_previous_summaries_and_custom_instructions_in_prompts() {
    let messages = vec![user_message("Summarize this.")];
    let (models, calls) = scripted_models(vec![faux_assistant_message(
        vec![ContentBlock::text("## Goal\nTest summary")],
        FauxAssistantOptions::default(),
    )]);
    let model = faux_model(false, 8192);
    let summary = generate_summary_with_usage(
        &messages,
        &models,
        &model,
        2000,
        None,
        Some("focus"),
        Some("old summary"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(summary.0.contains("Test summary"));
    let prompt = &calls.lock().unwrap()[0].0;
    assert!(prompt.contains("<previous-summary>\nold summary\n</previous-summary>"));
    assert!(prompt.contains("Additional focus: focus"));
}

#[tokio::test]
async fn returns_errors_for_failed_or_aborted_summary_generations() {
    let messages = vec![user_message("Summarize this.")];
    let model = faux_model(false, 8192);

    let (models, _) = scripted_models(vec![faux_assistant_message(
        vec![],
        FauxAssistantOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some("boom".into()),
        },
    )]);
    let err = generate_summary(
        &messages, &models, &model, 2000, None, None, None, None, None, None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "summarization_failed");
    assert_eq!(err.message, "Summarization failed: boom");

    let (models, _) = scripted_models(vec![faux_assistant_message(
        vec![],
        FauxAssistantOptions {
            stop_reason: Some(StopReason::Aborted),
            error_message: Some("stopped".into()),
        },
    )]);
    let err = generate_summary(
        &messages, &models, &model, 2000, None, None, None, None, None, None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "aborted");
    assert_eq!(err.message, "stopped");
}

#[tokio::test]
async fn clamps_max_tokens_and_isolates_requests() {
    let messages = vec![user_message("Summarize this.")];
    let (models, calls) = scripted_models(vec![
        faux_assistant_message(
            vec![ContentBlock::text("## Goal\nTest summary")],
            FauxAssistantOptions::default(),
        ),
        faux_assistant_message(
            vec![ContentBlock::text("## Goal\nTest summary")],
            FauxAssistantOptions::default(),
        ),
    ]);
    let model = faux_model(false, 128_000);
    let preparation = CompactionPreparation {
        messages_to_summarize: messages.clone(),
        turn_prefix_messages: messages.clone(),
        retained_tail: messages,
        is_split_turn: true,
        tokens_before: 600_000,
        previous_summary: None,
        file_ops: pi_agent::harness::compaction::utils::create_file_ops(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 500_000,
            keep_recent_tokens: 20_000,
        },
    };
    compact(&preparation, &models, &model, None, None, None, None, None)
        .await
        .unwrap();
    let captured = calls.lock().unwrap().clone();
    let max_tokens: Vec<_> = captured.iter().map(|(_, o)| o.base.max_tokens).collect();
    assert_eq!(max_tokens, vec![Some(128_000), Some(128_000)]);
    let cache: Vec<_> = captured
        .iter()
        .map(|(_, o)| o.base.cache_retention.clone())
        .collect();
    assert_eq!(
        cache,
        vec![Some("none".to_string()), Some("none".to_string())]
    );
    let session_ids: Vec<_> = captured
        .iter()
        .map(|(_, o)| o.base.session_id.clone())
        .collect();
    assert_ne!(session_ids[0], session_ids[1]);
    let both = session_ids.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(both.len(), 2);
}

#[tokio::test]
async fn combines_usage_for_split_turn_compaction() {
    let messages = vec![user_message("Summarize this.")];
    let model = faux_model(false, 8192);
    let history_usage = mock_usage(1, 2, 3, 4);
    let turn_prefix_usage = mock_usage(5, 6, 7, 8);
    let (models, _) = scripted_models(vec![
        {
            let mut m = faux_assistant_message(
                vec![ContentBlock::text("history summary")],
                FauxAssistantOptions::default(),
            );
            m.set_usage(history_usage);
            m
        },
        {
            let mut m = faux_assistant_message(
                vec![ContentBlock::text("turn prefix summary")],
                FauxAssistantOptions::default(),
            );
            m.set_usage(turn_prefix_usage);
            m
        },
    ]);
    let preparation = CompactionPreparation {
        messages_to_summarize: messages.clone(),
        turn_prefix_messages: messages,
        retained_tail: vec![],
        is_split_turn: true,
        tokens_before: 100,
        previous_summary: None,
        file_ops: pi_agent::harness::compaction::utils::create_file_ops(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };
    let result = compact(&preparation, &models, &model, None, None, None, None, None)
        .await
        .unwrap();
    let usage = result.usage.unwrap();
    assert_eq!(usage.input, 6);
    assert_eq!(usage.output, 8);
    assert_eq!(usage.cache_read, 10);
    assert_eq!(usage.cache_write, 12);
    assert_eq!(usage.total_tokens, 36);
}

#[tokio::test]
async fn passes_reasoning_through_turn_prefix_summaries_when_enabled() {
    let messages = vec![user_message("Summarize this.")];
    let (models, calls) = scripted_models(vec![faux_assistant_message(
        vec![ContentBlock::text("## Original Request\nTest summary")],
        FauxAssistantOptions::default(),
    )]);
    let model = faux_model(true, 8192);
    let preparation = CompactionPreparation {
        messages_to_summarize: vec![],
        turn_prefix_messages: messages,
        retained_tail: vec![],
        is_split_turn: true,
        tokens_before: 100,
        previous_summary: None,
        file_ops: pi_agent::harness::compaction::utils::create_file_ops(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 2000,
            keep_recent_tokens: 20,
        },
    };
    compact(
        &preparation,
        &models,
        &model,
        None,
        None,
        Some("high"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        calls.lock().unwrap()[0].1.reasoning,
        Some(pi_ai::types::ThinkingLevel::High)
    );
}

#[tokio::test]
async fn returns_compaction_result_with_file_details() {
    let u1 = message_entry(user_message("read a file"), "u1", None, 1);
    let a1_message = {
        let mut am = pi_ai::types::AssistantMessage::new();
        *am.content_mut() = vec![ContentBlock::tool_call(
            "tool-1",
            "read",
            serde_json::json!({"path": "src/index.ts"}),
        )];
        am.set_api_provider_model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
        am.set_usage(mock_usage(1000, 200, 0, 0));
        am.set_stop_reason(StopReason::Stop);
        AgentMessage::Core(Message::Assistant(am))
    };
    let a1 = message_entry(a1_message, "a1", Some("u1"), 2);
    let u2 = message_entry(user_message("continue"), "u2", Some("a1"), 3);
    let a2 = message_entry(
        assistant_message("done", Some(mock_usage(4000, 500, 0, 0))),
        "a2",
        Some("u2"),
        4,
    );
    let path_entries = [u1, a1, u2, a2];
    let budget = CompactionSettings {
        enabled: true,
        reserve_tokens: 2000,
        keep_recent_tokens: 1,
    };
    let preparation = prepare_compaction(&path_entries, &budget).unwrap().unwrap();
    assert!(preparation.is_split_turn);
    let (models, _) = scripted_models(vec![
        faux_assistant_message(
            vec![ContentBlock::text("## Goal\nTest summary")],
            FauxAssistantOptions::default(),
        ),
        faux_assistant_message(
            vec![ContentBlock::text("## Original Request\nTest suffix")],
            FauxAssistantOptions::default(),
        ),
    ]);
    let model = faux_model(false, 8192);
    let result = compact(&preparation, &models, &model, None, None, None, None, None)
        .await
        .unwrap();
    assert!(!result.summary.is_empty());
    assert!(result.usage.is_some());
    assert!(!result.retained_tail.is_empty());
    let details = result.details.unwrap();
    assert!(details.read_files.contains(&"src/index.ts".to_string()));
}

#[tokio::test]
async fn branch_summary_collects_abandoned_side_in_chronological_order() {
    let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(SessionMetadata {
        id: "session".into(),
        created_at: 1,
        cwd: "/tmp".into(),
        path: "memory://session".into(),
        modified_at: 1,
        source_format: 4,
        parent_session_id: None,
        legacy_parent_session_path: None,
        metadata: None,
    })));
    let mut session = Session::<MemoryFs>::from_in_memory(storage);
    let root_id = session.append_message(user_message("root")).await.unwrap();
    let common_id = session
        .append_message(user_message("common"))
        .await
        .unwrap();
    let abandoned_1 = session
        .append_message(user_message("abandoned 1"))
        .await
        .unwrap();
    let abandoned_2 = session
        .append_message(user_message("abandoned 2"))
        .await
        .unwrap();
    session
        .create_lane("target", Some(&common_id))
        .await
        .unwrap();
    let target_id = session
        .append_message_to_lane("target", user_message("target"))
        .await
        .unwrap();

    let result = collect_entries_for_branch_summary(&session, Some(&abandoned_2), &target_id)
        .await
        .unwrap();
    assert_eq!(
        result.common_ancestor_id.as_deref(),
        Some(common_id.as_str())
    );
    let ids: Vec<String> = result.entries.iter().map(|e| e.id().to_string()).collect();
    assert_eq!(ids, vec![abandoned_1, abandoned_2]);
    assert!(!ids.contains(&root_id));
}

#[tokio::test]
async fn branch_summary_returns_no_entries_without_previous_leaf() {
    let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(SessionMetadata {
        id: "session".into(),
        created_at: 1,
        cwd: "/tmp".into(),
        path: "memory://session".into(),
        modified_at: 1,
        source_format: 4,
        parent_session_id: None,
        legacy_parent_session_path: None,
        metadata: None,
    })));
    let mut session = Session::<MemoryFs>::from_in_memory(storage);
    let target_id = session
        .append_message(user_message("target"))
        .await
        .unwrap();
    let result = collect_entries_for_branch_summary(&session, None, &target_id)
        .await
        .unwrap();
    assert!(result.entries.is_empty());
    assert_eq!(result.common_ancestor_id, None);
}

#[tokio::test]
async fn generate_branch_summary_empty_returns_no_content_to_summarize() {
    let model = faux_model(false, 8192);
    let (models, _) = scripted_models(vec![]);
    let result = generate_branch_summary(
        &[],
        &models,
        &model,
        &GenerateBranchSummaryOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.summary, "No content to summarize");
}

#[tokio::test]
async fn generate_branch_summary_prepends_preamble_and_appends_file_ops() {
    let entries = vec![message_entry(
        {
            let mut am = pi_ai::types::AssistantMessage::new();
            *am.content_mut() = vec![ContentBlock::tool_call(
                "t1",
                "write",
                serde_json::json!({"path": "new.ts"}),
            )];
            am.set_api_provider_model("anthropic-messages", "anthropic", "claude-sonnet-4-5");
            am.set_usage(mock_usage(1, 1, 0, 0));
            am.set_stop_reason(StopReason::Stop);
            AgentMessage::Core(Message::Assistant(am))
        },
        "a1",
        None,
        1,
    )];
    let model = faux_model(false, 8192);
    let (models, _) = scripted_models(vec![faux_assistant_message(
        vec![ContentBlock::text("## Goal\nBranch work")],
        FauxAssistantOptions::default(),
    )]);
    let result = generate_branch_summary(
        &entries,
        &models,
        &model,
        &GenerateBranchSummaryOptions::default(),
    )
    .await
    .unwrap();
    assert!(result
        .summary
        .starts_with("The user explored a different conversation branch before returning here."));
    assert!(result.summary.contains("## Goal"));
    assert!(result
        .summary
        .contains("<modified-files>\nnew.ts\n</modified-files>"));
    assert_eq!(result.modified_files, vec!["new.ts".to_string()]);
}

#[test]
fn prepare_branch_entries_respects_token_budget() {
    // 4 user messages (8 chars each => 2 tokens); budget 4 keeps the newest ~2.
    let mut entries: Vec<Entry> = Vec::new();
    let mut prev: Option<String> = None;
    for i in 0..4 {
        let id = format!("u{i}");
        let parent = prev.clone();
        entries.push(message_entry(
            user_message(&format!("message {i}")),
            &id,
            parent.as_deref(),
            i as u64 + 1,
        ));
        prev = Some(id);
    }
    // Each message is 9 chars => ceil(9/4) = 3 tokens. Budget 4 keeps the
    // newest single message (3 tokens); the next push would exceed the budget.
    let prep = prepare_branch_entries(&entries, 4);
    assert_eq!(prep.messages.len(), 1);
    assert_eq!(prep.total_tokens, 3);
}
