#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `packages/agent/test/harness/session/jsonl-codec.test.ts`.

use pi_agent::session::jsonl::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation,
    JsonlDecodeErrorKind,
};
use pi_agent::session::types::{Entry, Fact, JsonlV4Header, Mutation};

fn header_round_trip(header: JsonlV4Header) {
    let encoded = encode_header(&header).unwrap();
    assert!(encoded.ends_with('\n'));
    let parsed = parse_header(encoded.trim_end()).unwrap();
    assert_eq!(parsed, header);
}

#[test]
fn round_trips_header_with_resolved_parent() {
    header_round_trip(JsonlV4Header {
        kind: "header".into(),
        version: 4,
        id: "session".into(),
        created_at: 1_700_000_000_000,
        cwd: "/workspace/project".into(),
        parent_session_id: Some("parent".into()),
        legacy_parent_session_path: None,
        metadata: Some(serde_json::json!({
            "owner": "agent",
            "nested": {"enabled": true},
            "values": [1, null, "two"],
        })),
    });
}

#[test]
fn round_trips_header_with_legacy_parent_path() {
    header_round_trip(JsonlV4Header {
        kind: "header".into(),
        version: 4,
        id: "legacy-child".into(),
        created_at: 1_700_000_000_001,
        cwd: "/workspace/project".into(),
        parent_session_id: None,
        legacy_parent_session_path: Some("/sessions/missing-parent.jsonl".into()),
        metadata: None,
    });
}

#[test]
fn projects_header_into_metadata() {
    let header = JsonlV4Header {
        kind: "header".into(),
        version: 4,
        id: "session".into(),
        created_at: 1_700_000_000_000,
        cwd: "/workspace/project".into(),
        parent_session_id: None,
        legacy_parent_session_path: Some("/sessions/missing-parent.jsonl".into()),
        metadata: Some(serde_json::json!({"owner": "agent"})),
    };
    assert_eq!(
        metadata_from_header(&header, "/sessions/session.jsonl", 1_700_000_000_100),
        pi_agent::session::types::SessionMetadata {
            id: "session".into(),
            created_at: 1_700_000_000_000,
            cwd: "/workspace/project".into(),
            path: "/sessions/session.jsonl".into(),
            modified_at: 1_700_000_000_100,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: Some("/sessions/missing-parent.jsonl".into()),
            metadata: Some(serde_json::json!({"owner": "agent"})),
        }
    );
}

fn mutation_round_trip(mutation: Mutation) {
    let encoded = encode_mutation(&mutation).unwrap();
    assert!(encoded.ends_with('\n'));
    let parsed = parse_mutation(encoded.trim_end()).unwrap();
    assert_eq!(parsed, mutation);
}

#[test]
fn returns_syntax_and_schema_errors() {
    let syntax = parse_mutation("{");
    assert!(syntax.is_err());
    assert_eq!(syntax.unwrap_err().kind, JsonlDecodeErrorKind::Syntax);

    let schema = parse_mutation(r#"{"kind": "unknown", "seq": 1}"#);
    assert!(schema.is_err());
    assert_eq!(schema.unwrap_err().kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn round_trips_lane_bound_entry_line() {
    mutation_round_trip(Mutation::Entry {
        lane: Some("main".into()),
        entry: Entry::Custom {
            id: "entry-1".into(),
            seq: 1,
            parent_id: None,
            timestamp: 100,
            custom_type: "note".into(),
            data: Some(serde_json::json!({"text": "hello"})),
        },
    });
}

#[test]
fn round_trips_imported_entry_without_lane() {
    mutation_round_trip(Mutation::Entry {
        lane: None,
        entry: Entry::Custom {
            id: "entry-1".into(),
            seq: 1,
            parent_id: None,
            timestamp: 100,
            custom_type: "note".into(),
            data: None,
        },
    });
}

#[test]
fn round_trips_record_line() {
    mutation_round_trip(Mutation::Record {
        record: pi_agent::session::types::LaneRecord::OperationStarted {
            id: "run-1".into(),
            seq: 1,
            lane: "main".into(),
            timestamp: 100,
            source_leaf_id: None,
            intent: pi_agent::session::types::OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        },
    });
}

#[test]
fn round_trips_lane_line() {
    mutation_round_trip(Mutation::Lane {
        seq: 1,
        lane: "thread".into(),
        leaf_id: Some("entry-1".into()),
    });
}

#[test]
fn round_trips_fact_lines_including_cleared_values() {
    mutation_round_trip(Mutation::Fact(Fact::Name {
        seq: 1,
        name: Some("Example".into()),
    }));
    mutation_round_trip(Mutation::Fact(Fact::Name { seq: 2, name: None }));
    mutation_round_trip(Mutation::Fact(Fact::Label {
        seq: 3,
        target_id: "entry-1".into(),
        label: Some("checkpoint".into()),
    }));
}

#[test]
fn rejects_custom_entry_without_custom_type() {
    let line =
        r#"{"kind":"entry","type":"custom","id":"entry","parentId":null,"seq":1,"timestamp":1}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn rejects_operation_started_without_intent() {
    let line = r#"{"kind":"record","type":"operation_started","id":"run","lane":"main","seq":1,"timestamp":1,"sourceLeafId":null}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn rejects_operation_finished_without_run_id() {
    let line = r#"{"kind":"record","type":"operation_finished","id":"finish","lane":"main","seq":1,"timestamp":1,"outcome":"completed"}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn rejects_missing_seq() {
    let line = r#"{"kind":"lane","lane":"main","leafId":null}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn rejects_message_entry_with_invalid_message() {
    let line = r#"{"kind":"entry","type":"message","id":"m","parentId":null,"seq":1,"timestamp":1,"message":{"role":"bogus"}}"#;
    assert!(parse_mutation(line).is_err());
}

#[test]
fn round_trips_message_entry_with_typed_message() {
    let line = r#"{"kind":"entry","lane":"main","type":"message","id":"m1","parentId":null,"seq":1,"timestamp":10,"terminate":true,"message":{"role":"user","content":[{"type":"text","text":"question"}],"timestamp":1}}"#;
    let parsed = parse_mutation(line).unwrap();
    match &parsed {
        Mutation::Entry {
            entry: Entry::Message {
                message, terminate, ..
            },
            ..
        } => {
            assert_eq!(terminate, &Some(true));
            // user message with one text block "question"
            match message {
                pi_agent::types::AgentMessage::Core(pi_ai::types::Message::User(u)) => {
                    match u.content() {
                        pi_ai::types::UserContentBody::Blocks(blocks) => {
                            assert_eq!(blocks.len(), 1);
                            assert!(
                                matches!(&blocks[0], pi_ai::types::ContentBlock::Text { text, .. } if text == "question")
                            );
                        }
                        other => panic!("expected blocks, got {other:?}"),
                    }
                }
                other => panic!("expected user core message, got {other:?}"),
            }
        }
        other => panic!("expected message entry, got {other:?}"),
    }
    let encoded = encode_mutation(&parsed).unwrap();
    assert!(encoded.contains("\"role\":\"user\""));
}
