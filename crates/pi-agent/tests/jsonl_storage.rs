//! Port of the storage round-trip cases from
//! `packages/agent/test/harness/session/jsonl-storage.test.ts`
//! (entries + branch queries; records + recovery projection + stats; facts).

use pi_agent::fs::{FileSystem, MemoryFs};
use pi_agent::session::jsonl::storage::JsonlSessionStorage;
use pi_agent::session::state::{EntryCursor, EntryOrder, EntryQuery, RecordQuery};
use pi_agent::session::types::{
    Entry, EntryNoStats, JsonlV4Header, LaneRecord, NewRecord, OperationIntent,
};
use pi_ai::types::{ContentBlock, Message, Usage, UserContent};

fn user_message(text: &str) -> pi_agent::types::AgentMessage {
    pi_agent::types::AgentMessage::Core(Message::User(UserContent::blocks(
        vec![ContentBlock::text(text)],
        1,
    )))
}

fn create_usage(multiplier: i64) -> Usage {
    Usage {
        input: multiplier,
        output: multiplier * 2,
        cache_read: multiplier * 3,
        cache_write: multiplier * 4,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: multiplier * 10,
        cost: pi_ai::types::Cost {
            input: multiplier as f64 * 0.1,
            output: multiplier as f64 * 0.2,
            cache_read: multiplier as f64 * 0.3,
            cache_write: multiplier as f64 * 0.4,
            total: multiplier as f64,
        },
    }
}

fn header(id: &str, cwd: &str) -> JsonlV4Header {
    JsonlV4Header {
        kind: "header".into(),
        version: 4,
        id: id.into(),
        created_at: 1_700_000_000_000,
        cwd: cwd.into(),
        parent_session_id: None,
        legacy_parent_session_path: None,
        metadata: None,
    }
}

fn enter_message(text: &str, id: &str, _timestamp: u64) -> EntryNoStats {
    EntryNoStats::Message {
        id: id.into(),
        message: user_message(text),
        terminate: None,
    }
}

#[test]
fn round_trips_every_entry_type_and_bounded_branch_queries() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut session = JsonlSessionStorage::create(
            fs.clone(),
            "/sessions/entries.jsonl",
            header("entries", "/work"),
        )
        .await
        .unwrap();

        let mut committed: Vec<Entry> = Vec::new();
        committed.push(
            session
                .append_entry(enter_message("question", "message", 1), "main")
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::Message {
                        id: "assistant-tool-call".into(),
                        message: pi_agent::types::AgentMessage::Core(Message::Assistant({
                            let mut m = pi_ai::types::AssistantMessage::new();
                            m.set_api_provider_model(
                                "anthropic-messages",
                                "anthropic",
                                "claude-sonnet-4-5",
                            );
                            m.content_mut().push(ContentBlock::text("I'll inspect it."));
                            m.content_mut().push(ContentBlock::tool_call(
                                "call-1",
                                "read",
                                serde_json::json!({"path": "README.md"}),
                            ));
                            m.set_usage(create_usage(1));
                            m.set_stop_reason(pi_ai::types::StopReason::ToolUse);
                            m.with_timestamp(2)
                        })),
                        terminate: None,
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::Message {
                        id: "tool-result".into(),
                        message: pi_agent::types::AgentMessage::Core(Message::ToolResult(
                            pi_ai::types::ToolResultMessage::new(
                                "call-1",
                                "read",
                                vec![ContentBlock::text("contents")],
                                false,
                            )
                            .with_details_usage_timestamp(
                                Some(create_usage(2)),
                                Some(serde_json::json!({"path": "README.md"})),
                                3,
                            ),
                        )),
                        terminate: Some(true),
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::ModelChange {
                        id: "model".into(),
                        provider: "anthropic".into(),
                        model_id: "claude-sonnet-4-5".into(),
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::ThinkingLevel {
                        id: "thinking".into(),
                        thinking_level: "high".into(),
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::ActiveTools {
                        id: "tools".into(),
                        active_tool_names: vec!["read".into(), "bash".into()],
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::Compaction {
                        id: "compaction".into(),
                        summary: "summary".into(),
                        retained_tail: vec![user_message("retained")],
                        tokens_before: 123,
                        details: Some(serde_json::json!({"source": "test"})),
                        usage: Some(create_usage(1)),
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::BranchSummary {
                        id: "branch-summary".into(),
                        from_id: "message".into(),
                        summary: "branch".into(),
                        details: Some(serde_json::json!({"reason": "navigation"})),
                        usage: Some(create_usage(2)),
                    },
                    "main",
                )
                .await
                .unwrap(),
        );
        committed.push(
            session
                .append_entry(
                    EntryNoStats::Custom {
                        id: "custom".into(),
                        custom_type: "note".into(),
                        data: Some(serde_json::json!({"nested": {"value": 1}})),
                    },
                    "main",
                )
                .await
                .unwrap(),
        );

        // Reopen from the persisted file.
        let restored = JsonlSessionStorage::load(fs.clone(), "/sessions/entries.jsonl")
            .await
            .unwrap();
        assert_eq!(
            restored
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap(),
            committed
        );
        let on_branch = restored
            .find_entries_on_branch(
                &EntryQuery::default(),
                "custom",
                &pi_agent::session::state::BranchBounds {
                    stop_at_type: Some("compaction".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            on_branch
                .iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>(),
            vec!["custom", "branch-summary", "compaction"]
        );

        // Cursor + limit
        let after = committed[5].seq();
        let paged = restored
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                cursor: Some(EntryCursor { after_seq: after }),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            paged.iter().map(|e| e.id().to_string()).collect::<Vec<_>>(),
            vec!["compaction", "branch-summary"]
        );

        // customType filter
        let custom = restored
            .find_entries(&EntryQuery {
                custom_type: Some("note".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            custom
                .iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>(),
            vec!["custom"]
        );

        // Stats: 3 message entries.
        assert_eq!(
            restored.get_stats().await,
            pi_agent::session::types::SessionStats {
                message_count: 3,
                cached_tokens: 0,
                uncached_tokens: 0,
                total_tokens: 0,
                cost_total: 0.0,
            }
        );

        // Deep-clone isolation: mutating a returned entry does not affect storage.
        let mut custom = restored.get_entry("custom").await.unwrap();
        if let Entry::Custom { data, .. } = &mut custom {
            data.as_mut().unwrap()["nested"]["value"] = serde_json::json!(99);
        }
        let fresh = restored.get_entry("custom").await.unwrap();
        assert_eq!(fresh, committed.last().unwrap().clone());

        // Lane pointers chain entries on main.
        let lanes = restored.get_lanes().await;
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].lane, "main");
        assert_eq!(lanes[0].leaf_id.as_deref(), Some("custom"));
    });
}

#[test]
fn round_trips_records_facts_and_recovery() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let mut session = JsonlSessionStorage::create(fs.clone(), "/sessions/records.jsonl", header("records", "/work"))
            .await
            .unwrap();
        let anchor = session.append_custom_entry("anchor", None).await.unwrap();
        let anchor_id = anchor.id().to_string();
        let anchor_id_b = anchor_id.clone();

        let mut records: Vec<LaneRecord> = Vec::new();
        records.push(
            session
                .append_record(NewRecord::OperationStarted {
                    id: "run".into(),
                    lane: "main".into(),
                    source_leaf_id: Some(anchor_id.clone()),
                    intent: OperationIntent::Run {
                        original_prompt: vec![user_message("prompt")],
                        initial_messages: vec![enter_message("initial", "initial", 1)],
                        system_prompt_override: Some("system".into()),
                        resume_data: Some(std::collections::BTreeMap::from([(
                            "extension".into(),
                            serde_json::json!({"version": 1}),
                        )])),
                    },
                })
                .await
                .unwrap(),
        );
        records.push(
            session
                .append_record(NewRecord::QueueEnqueued {
                    id: "steer".into(),
                    lane: "main".into(),
                    queue: "steer".into(),
                    run_id: "run".into(),
                    target: serde_json::json!({"type": "message", "id": "steer-message", "message": {
                        "role": "user", "content": [{"type": "text", "text": "steer"}], "timestamp": 1
                    }}),
                })
                .await
                .unwrap(),
        );
        records.push(
            session
                .append_record(NewRecord::StepAttempt {
                    id: "assistant-attempt".into(),
                    lane: "main".into(),
                    run_id: "run".into(),
                    step: "assistant".into(),
                    attempt: 1,
                    result_entry_id: "assistant-result".into(),
                    compaction_reason: None,
                })
                .await
                .unwrap(),
        );
        records.push(
            session
                .append_record(NewRecord::StepAttempt {
                    id: "compaction-attempt".into(),
                    lane: "main".into(),
                    run_id: "compaction".into(),
                    step: "compaction".into(),
                    attempt: 1,
                    result_entry_id: "compaction-result".into(),
                    compaction_reason: Some("manual".into()),
                })
                .await
                .unwrap(),
        );
        records.push(
            session
                .append_record(NewRecord::AbortRequested {
                    id: "abort".into(),
                    lane: "main".into(),
                    run_id: "run".into(),
                })
                .await
                .unwrap(),
        );
        records.push(
            session
                .append_record(NewRecord::OperationFinished {
                    id: "run-finished".into(),
                    lane: "main".into(),
                    run_id: "run".into(),
                    outcome: "aborted".into(),
                    error: None,
                })
                .await
                .unwrap(),
        );
        records.push(
            session
                .append_record(NewRecord::Usage {
                    id: "assistant-usage".into(),
                    lane: "main".into(),
                    cause: "assistant".into(),
                    run_id: Some("run".into()),
                    entry_id: Some("assistant-result".into()),
                    attempt: Some(1),
                    stop_reason: Some("stop".into()),
                    tool_call_id: None,
                    details: None,
                    usage: create_usage(1),
                })
                .await
                .unwrap(),
        );

        // Facts
        session.set_name(Some("Example")).await.unwrap();
        session.set_label(&anchor_id_b, Some("checkpoint")).await.unwrap();

        // Reopen & compare
        let restored = JsonlSessionStorage::load(fs.clone(), "/sessions/records.jsonl").await.unwrap();
        assert_eq!(
            restored
                .find_records(&RecordQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() })
                .await
                .unwrap(),
            records
        );
        assert_eq!(
            restored
                .find_records(&RecordQuery { record_type: Some("operation_started".into()), operation_kind: Some("run".into()), limit: Some(1), ..Default::default() })
                .await
                .unwrap()
                .iter()
                .map(|r| r.id().to_string())
                .collect::<Vec<_>>(),
            vec!["run".to_string()]
        );
        assert_eq!(
            restored
                .find_records(&RecordQuery { run_id: Some("compaction".into()), order: Some(EntryOrder::OldestFirst), ..Default::default() })
                .await
                .unwrap()
                .iter()
                .map(|r| r.id().to_string())
                .collect::<Vec<_>>(),
            vec!["compaction-attempt".to_string()]
        );
        assert_eq!(restored.get_name().await.as_deref(), Some("Example"));
        assert_eq!(restored.get_label(&anchor_id_b).await.as_deref(), Some("checkpoint"));

        // Recovery: the saved name survives reload.
        let raw = fs.content("/sessions/records.jsonl").unwrap();
        assert!(raw.contains(r#""kind":"header","version":4"#));
        assert!(raw.contains(r#""fact":"name""#));
        assert!(raw.lines().last().unwrap().contains(r#""fact":"label""#));
    });
}

#[test]
fn load_repairs_torn_tail() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let content = format!(
            "{}\n{}\n{}",
            serde_json::to_string(&header("torn", "/work")).unwrap(),
            serde_json::to_string(&pi_agent::session::types::Mutation::Entry {
                lane: Some("main".into()),
                entry: Entry::Custom {
                    id: "ok".into(),
                    seq: 1,
                    parent_id: None,
                    timestamp: 1,
                    custom_type: "note".into(),
                    data: None,
                },
            })
            .unwrap(),
            "{\"kind\":\"entry\",\"torn" // unterminated partial append
        );
        fs.write_file("/sessions/torn.jsonl", &content).unwrap();

        let restored = JsonlSessionStorage::load(fs.clone(), "/sessions/torn.jsonl")
            .await
            .unwrap();
        assert_eq!(
            restored
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            1
        );
        // The repair publishes a full file (no .tmp left behind).
        assert!(!fs.exists("/sessions/torn.jsonl.tmp"));
    });
}

#[test]
fn load_repairs_unterminated_tail() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let fs = MemoryFs::new();
        let header = serde_json::to_string(&header("unterm", "/work")).unwrap();
        fs.write_file("/sessions/unterm.jsonl", &header).unwrap(); // no trailing newline

        let restored = JsonlSessionStorage::load(fs.clone(), "/sessions/unterm.jsonl")
            .await
            .unwrap();
        assert!(fs
            .content("/sessions/unterm.jsonl")
            .unwrap()
            .ends_with('\n'));
        assert_eq!(restored.get_name().await, None);
    });
}
