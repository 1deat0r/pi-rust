#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use pi_agent::fs::MemoryFs;
use pi_agent::harness::agent_harness::{AgentHarness, AgentHarnessOptions, SuspensionReason};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::types::{EntryNoStats, NewRecord, OperationIntent};
use pi_agent::session::Session;
use pi_agent::types::AgentMessage;
use pi_ai::providers::{FauxProviderCore, RegisterFauxProviderOptions};
use pi_ai::types::{AssistantMessage, ContentBlock, DeferredHandle, StopReason};

fn session(id: &str) -> Session<MemoryFs> {
    let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(in_memory_metadata(
        id, None,
    ))));
    Session::from_in_memory(storage)
}

fn model() -> pi_ai::model::Model {
    FauxProviderCore::new(&RegisterFauxProviderOptions::default())
        .get_model(None)
        .expect("faux model")
        .clone()
}

fn deferred_message(handle: DeferredHandle) -> AgentMessage {
    let mut assistant = AssistantMessage::new();
    assistant.set_api_provider_model("faux", "faux", "faux-1");
    *assistant.content_mut() = vec![ContentBlock::text("pending")];
    assistant.set_stop_reason(StopReason::Deferred);
    assistant.set_deferred(handle);
    AgentMessage::Core(pi_ai::types::Message::Assistant(assistant))
}

#[tokio::test(flavor = "current_thread")]
async fn restart_restores_exact_deferred_suspension_handle() {
    let mut recorded = session("deferred-restart");
    let handle = DeferredHandle {
        provider: "faux".to_string(),
        model_id: "faux-1".to_string(),
        api: "faux".to_string(),
        id: "deferred-handle".to_string(),
        expires_at: Some(12_345),
        poll_after_ms: Some(25),
        data: Some(serde_json::json!({"opaque": true})),
    };
    recorded
        .append_record(NewRecord::OperationStarted {
            id: "deferred-run".to_string(),
            lane: "main".to_string(),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        })
        .await
        .expect("operation start");
    recorded
        .append_record(NewRecord::StepAttempt {
            id: "deferred-step".to_string(),
            lane: "main".to_string(),
            run_id: "deferred-run".to_string(),
            step: "assistant".to_string(),
            attempt: 1,
            result_entry_id: "deferred-entry".to_string(),
            compaction_reason: None,
        })
        .await
        .expect("step attempt");
    recorded
        .append_entry(
            EntryNoStats::Message {
                id: "deferred-entry".to_string(),
                message: deferred_message(handle.clone()),
                terminate: None,
            },
            "main",
        )
        .await
        .expect("deferred assistant");

    let (_, suspended) = AgentHarness::create(AgentHarnessOptions::new(recorded, model()))
        .await
        .expect("restore harness");
    assert_eq!(suspended.len(), 1);
    assert_eq!(suspended[0].reason, SuspensionReason::Deferred);
    assert_eq!(suspended[0].deferred.as_ref(), Some(&handle));
}

#[tokio::test(flavor = "current_thread")]
async fn restart_keeps_non_deferred_open_operations_classified_as_crashes() {
    let mut recorded = session("crash-restart");
    recorded
        .append_record(NewRecord::OperationStarted {
            id: "crashed-run".to_string(),
            lane: "main".to_string(),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: None,
            },
        })
        .await
        .expect("operation start");

    let (_, suspended) = AgentHarness::create(AgentHarnessOptions::new(recorded, model()))
        .await
        .expect("restore harness");
    assert_eq!(suspended.len(), 1);
    assert_eq!(suspended[0].reason, SuspensionReason::Crash);
    assert!(suspended[0].deferred.is_none());
}
