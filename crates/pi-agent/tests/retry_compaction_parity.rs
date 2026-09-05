#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use pi_agent::fs::MemoryFs;
use pi_agent::harness::agent_harness::{AgentHarness, AgentHarnessOptions};
use pi_agent::rich_agent::RichAgentEvent;
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::state::{EntryOrder, EntryQuery, RecordQuery};
use pi_agent::session::types::{Entry, LaneRecord};
use pi_agent::session::Session;
use pi_agent::types::AgentMessage;
use pi_ai::providers::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pi_ai::types::{ContentBlock, Message, StopReason};
use pi_ai::utils::retry::RetryPolicy;

fn session(id: &str) -> Session<MemoryFs> {
    let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(in_memory_metadata(
        id, None,
    ))));
    Session::from_in_memory(storage)
}

#[tokio::test(flavor = "current_thread")]
async fn transient_retry_settles_one_durable_operation_without_failed_attempt_duplication() {
    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            Vec::new(),
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("overloaded_error".to_string()),
            },
        )),
        FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("retry recovered")],
            FauxAssistantOptions::default(),
        )),
    ]);
    let model = core.get_model(None).expect("faux model").clone();
    let stream_core = core.clone();
    let mut options = AgentHarnessOptions::new(session("retry-durable"), model);
    options.stream_fn = Some(Arc::new(move |model, context| {
        stream_core.stream(model, context, None)
    }));
    options.retry = Some(RetryPolicy {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 0,
    });
    let (harness, suspended) = AgentHarness::create(options).await.expect("create harness");
    assert!(suspended.is_empty());

    let (messages, events) = harness
        .run_prompt_with_events(vec![pi_agent::user_text_prompt("retry me", 1)])
        .await
        .expect("retry run");
    assert!(events.iter().any(|event| matches!(
        event,
        RichAgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 2,
            error_message,
            ..
        } if error_message == "overloaded_error"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RichAgentEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        }
    )));
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, AgentMessage::Core(Message::Assistant(_))))
            .count(),
        1
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::Core(Message::Assistant(assistant))
            if assistant.content().iter().any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text == "retry recovered"
            ))
    )));

    let session = harness.session();
    let locked = session.lock().await;
    let entries = locked
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("durable entries");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(
                entry,
                Entry::Message {
                    message: AgentMessage::Core(Message::User(_)),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(
                entry,
                Entry::Message {
                    message: AgentMessage::Core(Message::Assistant(_)),
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(!entries.iter().any(|entry| matches!(
        entry,
        Entry::Message {
            message: AgentMessage::Core(Message::Assistant(assistant)),
            ..
        } if assistant.error_message() == Some("overloaded_error")
    )));
    let records = locked
        .find_records(&RecordQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("durable records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, LaneRecord::OperationStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, LaneRecord::OperationFinished { .. }))
            .count(),
        1
    );
    assert_eq!(core.state.lock().unwrap().call_count, 2);
}
