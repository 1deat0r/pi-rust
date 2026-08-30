#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Port of `packages/evals/test/pi-harness.test.ts`.

use pi_evals::harness::{
    resolve_model_selection, session_id_from_session_jsonl, transcript_events_from_session_jsonl,
    ModelSelection, TranscriptEvent,
};

#[test]
fn prefers_an_explicit_harness_model_over_environment_defaults() {
    let selection = resolve_model_selection(
        Some(&ModelSelection {
            provider: "anthropic".into(),
            id: "claude-opus-4-6".into(),
        }),
        Some(("openai-codex", "gpt-5.6-sol")),
    )
    .unwrap();
    assert_eq!(
        selection,
        ModelSelection {
            provider: "anthropic".into(),
            id: "claude-opus-4-6".into()
        }
    );
}

#[test]
fn uses_trimmed_environment_defaults_when_the_harness_has_no_explicit_model() {
    let selection =
        resolve_model_selection(None, Some((" openai-codex ", " gpt-5.6-sol "))).unwrap();
    assert_eq!(
        selection,
        ModelSelection {
            provider: "openai-codex".into(),
            id: "gpt-5.6-sol".into()
        }
    );
}

#[test]
fn rejects_an_incomplete_model_selection() {
    for case in [
        (None, None),
        (None, Some(("openai-codex", ""))),
        (None, Some(("", "gpt-5.6-sol"))),
        (Some(""), Some(("gpt-5.6-sol", "x"))),
    ] {
        let explicit = case.0.map(|p| ModelSelection {
            provider: p.to_string(),
            id: "x".into(),
        });
        let env = case.1;
        match resolve_model_selection(explicit.as_ref(), env) {
            Ok(selection) => panic!("expected rejection, got {selection:?}"),
            Err(message) => assert_eq!(
                message.to_string(),
                "Select a harness model explicitly or set both PI_PROVIDER and PI_MODEL as defaults."
            ),
        }
    }
}

#[test]
fn reconstructs_transcript_events_from_durable_messages() {
    let session = include_str!("fixtures/session-usage-v4.jsonl");
    let events = transcript_events_from_session_jsonl(session).expect("transcript parses");

    assert!(matches!(
        &events[0],
        TranscriptEvent::Message { role, content }
            if role == "user" && content == "hello"
    ));
    assert!(matches!(
        &events[1],
        TranscriptEvent::Message { role, content }
            if role == "assistant" && content == "hi"
    ));
    assert!(matches!(
        &events[2],
        TranscriptEvent::ToolCall { id, name, arguments }
            if id == "call-1"
                && name == "hello"
                && arguments.as_ref().and_then(|v| v.get("name")).and_then(|v| v.as_str()) == Some("Bob")
    ));
    assert!(matches!(
        &events[3],
        TranscriptEvent::ToolResult { tool_call_id, name, content, error }
            if tool_call_id == "call-1"
                && name == "hello"
                && content == &serde_json::json!("Hello, Bob!")
                && error.is_empty()
    ));
    assert_eq!(
        session_id_from_session_jsonl(session).as_deref(),
        Some("eval-session")
    );
}

#[test]
fn malformed_transcript_jsonl_is_reported_instead_of_becoming_a_fake_trace() {
    let error =
        transcript_events_from_session_jsonl("{\"kind\":\"header\",\"version\":4}\nnot-json\n")
            .expect_err("malformed durable transcript must fail closed");
    assert!(
        error.to_string().contains("transcript JSONL line 2"),
        "{error}"
    );
}
