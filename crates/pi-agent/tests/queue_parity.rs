#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::fs::MemoryFs;
use pi_agent::harness::agent_harness::{
    AgentHarness, AgentHarnessOptions, AgentLane, CancelQueuedOutcome, QueueMode,
};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::Session;
use pi_agent::types::AgentMessage;
use pi_ai::providers::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, RegisterFauxProviderOptions,
};
use pi_ai::types::{
    AssistantMessageEvent, ContentBlock, Context, DoneReason, Message, UserContent,
};
use pi_ai::AssistantMessageEventStream;
use tokio::sync::{Notify, Semaphore};

fn session(id: &str) -> Session<MemoryFs> {
    let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(in_memory_metadata(
        id, None,
    ))));
    Session::from_in_memory(storage)
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::Core(Message::User(UserContent::string(text, 1)))
}

fn user_text(message: &Message) -> Option<String> {
    match message {
        Message::User(content) => Some(pi_agent::agent::user_content_text(content)),
        _ => None,
    }
}

struct QueueFixture {
    harness: Arc<AgentHarness<MemoryFs>>,
    started: Arc<Notify>,
    release_first: Arc<Semaphore>,
    contexts: Arc<Mutex<Vec<Context>>>,
    calls: Arc<AtomicUsize>,
}

async fn queue_fixture(id: &str, steering: QueueMode, follow_up: QueueMode) -> QueueFixture {
    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    let model = core.get_model(None).expect("faux model").clone();
    let started = Arc::new(Notify::new());
    let release_first = Arc::new(Semaphore::new(0));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_fn = {
        let started = started.clone();
        let release_first = release_first.clone();
        let contexts = contexts.clone();
        let calls = calls.clone();
        Arc::new(move |_model: &pi_ai::model::Model, context: &Context| {
            contexts.lock().unwrap().push(context.clone());
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            let stream = AssistantMessageEventStream::new();
            let sender = stream.sender().expect("stream sender");
            let started = started.clone();
            let release_first = release_first.clone();
            tokio::spawn(async move {
                if call == 1 {
                    started.notify_waiters();
                    let _permit = release_first.acquire().await.expect("release permit");
                }
                let _ = sender.send(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: faux_assistant_message(
                        vec![ContentBlock::text(format!("reply-{call}"))],
                        FauxAssistantOptions::default(),
                    ),
                });
            });
            stream
        })
    };
    let mut options = AgentHarnessOptions::new(session(id), model);
    options.stream_fn = Some(stream_fn);
    options.steering_mode = Some(steering);
    options.follow_up_mode = Some(follow_up);
    let (harness, suspended) = AgentHarness::create(options).await.expect("create harness");
    assert!(suspended.is_empty());
    QueueFixture {
        harness: Arc::new(harness),
        started,
        release_first,
        contexts,
        calls,
    }
}

fn request_new_user_texts(contexts: &[Context]) -> Vec<Vec<String>> {
    let mut previous = 0;
    contexts
        .iter()
        .map(|context| {
            let users = context.messages[previous..]
                .iter()
                .filter_map(user_text)
                .collect::<Vec<_>>();
            previous = context.messages.len();
            users
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn one_at_a_time_queues_preserve_order_cancel_exactly_once_and_allow_reuse() {
    let fixture = queue_fixture(
        "queue-one-at-a-time",
        QueueMode::OneAtATime,
        QueueMode::OneAtATime,
    )
    .await;
    let started = fixture.started.clone();
    let started_wait = started.notified();
    let running = fixture.harness.clone();
    let run =
        tokio::spawn(async move { running.run_prompt_with_events(vec![user("initial")]).await });
    tokio::time::timeout(Duration::from_secs(1), started_wait)
        .await
        .expect("first provider request started");

    let steer_one = AgentLane::steer_message(fixture.harness.as_ref(), &user("steer-1"))
        .await
        .expect("queue steer one");
    let steer_cancelled = AgentLane::steer_message(fixture.harness.as_ref(), &user("steer-cancel"))
        .await
        .expect("queue cancelled steer");
    AgentLane::follow_up_message(fixture.harness.as_ref(), &user("follow-1"))
        .await
        .expect("queue follow one");
    AgentLane::follow_up_message(fixture.harness.as_ref(), &user("follow-2"))
        .await
        .expect("queue follow two");
    assert_eq!(
        AgentLane::cancel_queued(fixture.harness.as_ref(), &steer_cancelled)
            .await
            .expect("cancel queued steer"),
        CancelQueuedOutcome::Cancelled
    );
    assert_ne!(steer_one, steer_cancelled);
    fixture.release_first.add_permits(1);

    let (messages, events) = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("queued run settled")
        .expect("run task")
        .expect("queued run result");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
    assert!(events
        .iter()
        .any(|event| matches!(event, pi_agent::rich_agent::RichAgentEvent::AgentEnd { .. })));
    let users = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Core(message) => user_text(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users, ["initial", "steer-1", "follow-1", "follow-2"]);
    let contexts = fixture.contexts.lock().unwrap().clone();
    assert_eq!(
        request_new_user_texts(&contexts),
        [["initial"], ["steer-1"], ["follow-1"], ["follow-2"]]
    );
    assert!(!contexts
        .iter()
        .flat_map(|context| &context.messages)
        .any(|message| { user_text(message).as_deref() == Some("steer-cancel") }));
    assert!(AgentLane::watch(fixture.harness.as_ref())
        .await
        .expect("queue snapshot")
        .snapshot
        .queues
        .steer
        .is_empty());

    fixture
        .harness
        .run_prompt(vec![user("reuse")])
        .await
        .expect("reuse after queue drain");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test(flavor = "current_thread")]
async fn all_mode_batches_each_queue_at_one_drain_boundary() {
    let fixture = queue_fixture("queue-all", QueueMode::All, QueueMode::All).await;
    let started = fixture.started.clone();
    let started_wait = started.notified();
    let running = fixture.harness.clone();
    let run = tokio::spawn(async move { running.run_prompt(vec![user("initial")]).await });
    tokio::time::timeout(Duration::from_secs(1), started_wait)
        .await
        .expect("first provider request started");

    for text in ["steer-1", "steer-2"] {
        AgentLane::steer_message(fixture.harness.as_ref(), &user(text))
            .await
            .expect("queue steering");
    }
    for text in ["follow-1", "follow-2"] {
        AgentLane::follow_up_message(fixture.harness.as_ref(), &user(text))
            .await
            .expect("queue follow-up");
    }
    fixture.release_first.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("all-mode run settled")
        .expect("run task")
        .expect("all-mode result");

    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    let contexts = fixture.contexts.lock().unwrap().clone();
    assert_eq!(
        request_new_user_texts(&contexts),
        [
            vec!["initial".to_string()],
            vec!["steer-1".to_string(), "steer-2".to_string()],
            vec!["follow-1".to_string(), "follow-2".to_string()],
        ]
    );
}
