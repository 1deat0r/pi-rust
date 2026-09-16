#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Wave B1 RED test: run-context + tool-invocation memos.

use pi_agent::harness::run_context::{AgentHarnessToolInvocation, RunContext};

#[test]
fn run_context_background_has_no_deadline_and_no_values() {
    let context = RunContext::background();
    assert!(!context.is_cancelled());
    assert!(context.deadline_ms().is_none());
    assert!(context.value("telemetry").is_none());
}

#[test]
fn run_context_with_value_round_trips_typed_values() {
    let context = RunContext::background().with_value("turn", serde_json::json!("42"));
    assert_eq!(
        context.value("turn").as_ref().and_then(|v| v.as_str()),
        Some("42")
    );
}

#[test]
fn run_context_cancel_propagates_to_children() {
    let parent = RunContext::background();
    let child = parent.child();
    parent.cancel();
    assert!(parent.is_cancelled());
    assert!(child.is_cancelled());
}

#[tokio::test]
async fn tool_invocation_memos_round_trip_per_invocation() {
    let first = AgentHarnessToolInvocation::new("inv-1", "op-1", "turn-1");
    let second = AgentHarnessToolInvocation::new("inv-2", "op-1", "turn-1");
    assert_eq!(first.invocation_id, "inv-1");
    assert!(first.get_memo("checkpoint").await.is_none());
    first
        .set_memo("checkpoint", Some(serde_json::json!({"step": 3})))
        .await;
    assert_eq!(
        first.get_memo("checkpoint").await,
        Some(serde_json::json!({"step": 3}))
    );
    // Memos are invocation-scoped: a sibling invocation sees nothing.
    assert!(second.get_memo("checkpoint").await.is_none());
    // Deleting with None clears the memo.
    first.set_memo("checkpoint", None).await;
    assert!(first.get_memo("checkpoint").await.is_none());
}
