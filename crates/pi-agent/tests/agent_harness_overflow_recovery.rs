#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! AgentHarness integration coverage for the real overflow-recovery seam.
//!
//! The provider transport is deterministic so the test can force the two
//! overflow branches offline. The recovery itself is not mocked: the hook
//! appends the failed response to a real Session, runs the real compaction
//! preparation/summarization functions, appends the resulting compaction
//! entry, and rebuilds the provider-facing context from that session.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::agent::StreamFn;
use pi_agent::fs::MemoryFs;
use pi_agent::harness::compaction::{compact, prepare_compaction, CompactionSettings};
use pi_agent::harness::{AgentHarness, AgentHarnessOptions, SimpleModels};
use pi_agent::rich_agent::{OverflowRecoveryHook, OverflowRecoveryResult};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::state::{EntryOrder, EntryQuery};
use pi_agent::session::types::{Entry, EntryNoStats};
use pi_agent::session::Session;
use pi_agent::types::AgentMessage;
use pi_ai::model::Model;
use pi_ai::providers::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pi_ai::types::{ContentBlock, Message, StopReason, Usage};

type SharedSession = Arc<tokio::sync::Mutex<Session<MemoryFs>>>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

fn shared_session(id: &str) -> (Session<MemoryFs>, SharedSession) {
    let storage = Arc::new(std::sync::Mutex::new(InMemorySessionStorage::new(
        in_memory_metadata(id, None),
    )));
    let harness_session = Session::from_in_memory(storage.clone());
    let hook_session = Arc::new(tokio::sync::Mutex::new(Session::from_in_memory(storage)));
    (harness_session, hook_session)
}

fn model() -> Model {
    Model::new("faux-1", "Faux Model", "faux", "faux")
}

fn usage(input: i64, output: i64) -> Usage {
    Usage {
        input,
        output,
        total_tokens: input.saturating_add(output),
        ..Default::default()
    }
}

fn assistant(
    text: &str,
    stop_reason: StopReason,
    error_message: Option<&str>,
) -> pi_ai::types::AssistantMessage {
    let mut response = faux_assistant_message(
        vec![ContentBlock::text(text)],
        FauxAssistantOptions {
            stop_reason: Some(stop_reason),
            error_message: error_message.map(str::to_string),
        },
    );
    response.set_usage(usage(16, 2));
    response
}

fn is_error(message: &AgentMessage, expected: &str) -> bool {
    matches!(
        message,
        AgentMessage::Core(Message::Assistant(response))
            if response.stop_reason() == Some(StopReason::Error)
                && response.error_message() == Some(expected)
    )
}

fn is_assistant_error(message: &Message, expected: &str) -> bool {
    matches!(
        message,
        Message::Assistant(response) if response.error_message() == Some(expected)
    )
}

fn session_backed_recovery_hook(
    session: SharedSession,
    summary_models: SimpleModels,
    settings: CompactionSettings,
    calls: Arc<AtomicUsize>,
    compaction_persisted: Arc<AtomicBool>,
) -> OverflowRecoveryHook {
    Arc::new(move |request, signal| {
        calls.fetch_add(1, Ordering::SeqCst);
        let session = session.clone();
        let summary_models = summary_models.clone();
        let settings = settings.clone();
        let compaction_persisted = compaction_persisted.clone();

        Box::pin(async move {
            let mut session = session.lock().await;

            // The hook owns the durable overflow response, as required by the
            // OverflowRecoveryHook contract. The harness's returned delta is
            // still delivered to its normal session owner afterward.
            for (index, message) in request.durable_messages.iter().cloned().enumerate() {
                session
                    .append_entry(
                        EntryNoStats::Message {
                            id: format!("overflow-durable-{index}"),
                            message,
                            terminate: None,
                        },
                        "main",
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }

            let entries = session
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .map_err(|error| error.to_string())?;
            let preparation = prepare_compaction(&entries, &settings)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "overflow recovery had no compaction preparation".to_string())?;
            let compacted = compact(
                &preparation,
                &summary_models,
                &request.model,
                None,
                signal.as_ref(),
                Some("off"),
                None,
                None,
            )
            .await
            .map_err(|error| error.to_string())?;

            let details = compacted.details.as_ref().map(|details| {
                serde_json::json!({
                    "readFiles": details.read_files,
                    "modifiedFiles": details.modified_files,
                })
            });
            session
                .append_entry(
                    EntryNoStats::Compaction {
                        id: "overflow-compaction".to_string(),
                        summary: compacted.summary,
                        retained_tail: compacted.retained_tail,
                        tokens_before: compacted.tokens_before,
                        details,
                        usage: compacted.usage,
                    },
                    "main",
                )
                .await
                .map_err(|error| error.to_string())?;
            compaction_persisted.store(true, Ordering::SeqCst);

            let rebuilt_entries = session
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .map_err(|error| error.to_string())?;
            let rebuilt = pi_agent::session::context::build_session_context(
                &rebuilt_entries,
                &Default::default(),
            );
            let mut context = request.context;
            context.messages = rebuilt.messages;
            Ok(OverflowRecoveryResult { context })
        })
    })
}

struct HarnessFixture {
    harness: AgentHarness<MemoryFs>,
    core: FauxProviderCore,
    provider_contexts: Arc<Mutex<Vec<Vec<Message>>>>,
    recovery_ready_at_request: Arc<Mutex<Vec<bool>>>,
    recovery_calls: Arc<AtomicUsize>,
    compaction_persisted: Arc<AtomicBool>,
    session: SharedSession,
}

async fn fixture(responses: Vec<FauxResponseStep>, id: &str) -> HarnessFixture {
    fixture_impl(responses, id, true).await
}

async fn default_fixture(responses: Vec<FauxResponseStep>, id: &str) -> HarnessFixture {
    fixture_impl(responses, id, false).await
}

async fn fixture_impl(
    responses: Vec<FauxResponseStep>,
    id: &str,
    install_custom_recovery: bool,
) -> HarnessFixture {
    let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
    core.set_responses(responses);
    let provider_contexts = Arc::new(Mutex::new(Vec::new()));
    let provider_contexts_for_stream = provider_contexts.clone();
    let recovery_ready_at_request = Arc::new(Mutex::new(Vec::new()));
    let recovery_ready_for_stream = recovery_ready_at_request.clone();
    let core_for_stream = core.clone();
    let compaction_persisted = Arc::new(AtomicBool::new(false));
    let compaction_state_for_stream = compaction_persisted.clone();
    let stream_fn: StreamFn = Arc::new(move |request_model, context| {
        provider_contexts_for_stream
            .lock()
            .expect("provider context lock")
            .push(context.messages.clone());
        recovery_ready_for_stream
            .lock()
            .expect("recovery state lock")
            .push(compaction_state_for_stream.load(Ordering::SeqCst));
        core_for_stream.stream(request_model, context, None)
    });
    let (harness_session, session) = shared_session(id);
    let recovery_calls = Arc::new(AtomicUsize::new(0));
    let settings = CompactionSettings {
        enabled: true,
        reserve_tokens: 64,
        keep_recent_tokens: 1,
    };
    let mut options = AgentHarnessOptions::new(harness_session, model());
    options.stream_fn = Some(stream_fn);
    if install_custom_recovery {
        let summary_stream = options.stream_fn.clone().expect("stream function");
        let summary_models = SimpleModels::new(move |request_model, context, _options| {
            let stream = summary_stream(request_model, context);
            Box::pin(async move { stream.collect().await.1 })
        });
        options.overflow_recovery = Some(session_backed_recovery_hook(
            session.clone(),
            summary_models,
            settings.clone(),
            recovery_calls.clone(),
            compaction_persisted.clone(),
        ));
    }
    options.compaction = Some(CompactionSettings {
        enabled: true,
        reserve_tokens: 64,
        keep_recent_tokens: 1,
    });
    let (harness, suspended) = AgentHarness::create(options)
        .await
        .expect("harness should create");
    assert!(suspended.is_empty());
    HarnessFixture {
        harness,
        core,
        provider_contexts,
        recovery_ready_at_request,
        recovery_calls,
        compaction_persisted,
        session,
    }
}

fn overflow_one() -> FauxResponseStep {
    FauxResponseStep::Message(assistant(
        "provider rejected the request",
        StopReason::Error,
        Some("prompt is too long for the model context window"),
    ))
}

fn overflow_two() -> FauxResponseStep {
    FauxResponseStep::Message(assistant(
        "provider rejected the retry",
        StopReason::Error,
        Some("retry still exceeds the context window"),
    ))
}

fn summary() -> FauxResponseStep {
    FauxResponseStep::Message(assistant(
        "## Goal\nPreserve the durable conversation checkpoint.",
        StopReason::Stop,
        None,
    ))
}

#[test]
fn harness_recovers_overflow_with_real_session_compaction_and_retry() {
    runtime().block_on(async {
        let fixture = fixture(
            vec![
                overflow_one(),
                summary(),
                FauxResponseStep::Message(assistant(
                    "retry succeeded",
                    StopReason::Stop,
                    None,
                )),
            ],
            "harness-overflow-recovery",
        )
        .await;

        let returned = fixture
            .harness
            .run_prompt(vec![pi_agent::user_text_prompt("recover this request", 1)])
            .await
            .expect("harness run should settle");

        assert_eq!(fixture.recovery_calls.load(Ordering::SeqCst), 1);
        assert!(fixture.compaction_persisted.load(Ordering::SeqCst));
        assert!(returned.iter().any(|message| is_error(
            message,
            "prompt is too long for the model context window"
        )));
        assert!(returned.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Core(Message::Assistant(response))
                    if response.content().iter().any(|block| {
                        matches!(block, ContentBlock::Text { text, .. } if text == "retry succeeded")
                    })
            )
        }));

        let (context_count, retry_context) = {
            let contexts = fixture
                .provider_contexts
                .lock()
                .expect("provider context lock");
            (contexts.len(), contexts[2].clone())
        };
        assert_eq!(context_count, 3, "initial, summary, and retry requests");
        assert_eq!(
            *fixture
                .recovery_ready_at_request
                .lock()
                .expect("recovery state lock"),
            vec![false, false, true]
        );
        assert!(!retry_context.iter().any(|message| {
            is_assistant_error(
                message,
                "prompt is too long for the model context window",
            )
        }));

        let entries = fixture
            .session
            .lock()
            .await
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("session transcript");
        assert!(entries.iter().any(|entry| {
            matches!(entry, Entry::Compaction { summary, .. } if summary.contains("Preserve the durable conversation checkpoint"))
        }));
        assert!(entries.iter().any(|entry| {
            matches!(
                entry,
                Entry::Message { message, .. }
                    if is_error(message, "prompt is too long for the model context window")
            )
        }));
        assert!(entries.iter().any(|entry| {
            matches!(
                entry,
                Entry::Message { message, .. }
                    if matches!(
                        message,
                        AgentMessage::Core(Message::Assistant(response))
                            if response.content().iter().any(|block| {
                                matches!(block, ContentBlock::Text { text, .. } if text == "retry succeeded")
                            })
                    )
            )
        }));
        assert_eq!(fixture.core.get_pending_response_count(), 0);
    });
}

#[test]
fn harness_stops_after_the_second_overflow_without_a_second_recovery() {
    runtime().block_on(async {
        let fixture = fixture(
            vec![overflow_one(), summary(), overflow_two()],
            "harness-second-overflow",
        )
        .await;

        let returned = fixture
            .harness
            .run_prompt(vec![pi_agent::user_text_prompt("do not loop", 1)])
            .await
            .expect("harness run should settle with terminal provider error");

        assert_eq!(fixture.recovery_calls.load(Ordering::SeqCst), 1);
        assert!(returned.iter().any(|message| {
            is_error(message, "prompt is too long for the model context window")
        }));
        assert!(returned
            .iter()
            .any(|message| { is_error(message, "retry still exceeds the context window") }));

        let (context_count, retry_context) = {
            let contexts = fixture
                .provider_contexts
                .lock()
                .expect("provider context lock");
            (contexts.len(), contexts[2].clone())
        };
        assert_eq!(context_count, 3, "initial, summary, and terminal retry");
        assert_eq!(
            *fixture
                .recovery_ready_at_request
                .lock()
                .expect("recovery state lock"),
            vec![false, false, true]
        );
        assert!(!retry_context.iter().any(|message| {
            is_assistant_error(message, "prompt is too long for the model context window")
        }));

        let entries = fixture
            .session
            .lock()
            .await
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("session transcript");
        assert!(entries
            .iter()
            .any(|entry| { matches!(entry, Entry::Compaction { .. }) }));
        assert!(entries.iter().any(|entry| {
            matches!(
                entry,
                Entry::Message { message, .. }
                    if is_error(message, "retry still exceeds the context window")
            )
        }));
        assert_eq!(fixture.core.get_pending_response_count(), 0);
    });
}

#[test]
fn harness_installs_real_durable_overflow_recovery_by_default() {
    runtime().block_on(async {
        let fixture = default_fixture(
            vec![
                overflow_one(),
                summary(),
                FauxResponseStep::Message(assistant("default retry succeeded", StopReason::Stop, None)),
            ],
            "harness-default-overflow-recovery",
        )
        .await;

        let returned = fixture
            .harness
            .run_prompt(vec![pi_agent::user_text_prompt("default recovery", 1)])
            .await
            .expect("default recovery should settle");
        assert!(returned.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Core(Message::Assistant(response))
                    if response.content().iter().any(|block| {
                        matches!(block, ContentBlock::Text { text, .. } if text == "default retry succeeded")
                    })
            )
        }));
        assert_eq!(
            fixture
                .provider_contexts
                .lock()
                .expect("provider context lock")
                .len(),
            3,
            "the default hook must use the real stream for summary and retry"
        );

        let entries = fixture
            .session
            .lock()
            .await
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .expect("session transcript");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry, Entry::Compaction { .. }))
                .count(),
            1,
            "default recovery must persist one compaction boundary"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry,
                    Entry::Message { message, .. }
                        if matches!(message, AgentMessage::Core(Message::User(_)))
                ))
                .count(),
            1,
            "the prompt must not be appended twice after the hook persists it"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry,
                    Entry::Message { message, .. }
                        if is_error(message, "prompt is too long for the model context window")
                ))
                .count(),
            1,
            "the failed response must not be appended twice"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry,
                    Entry::Message { message, .. }
                        if matches!(
                            message,
                            AgentMessage::Core(Message::Assistant(response))
                                if response.content().iter().any(|block| {
                                    matches!(block, ContentBlock::Text { text, .. } if text == "default retry succeeded")
                                })
                        )
                ))
                .count(),
            1,
            "the retry response must be persisted once"
        );
    });
}

#[test]
fn configured_overflow_hook_is_propagated_to_created_lanes() {
    runtime().block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        core.set_responses(vec![
            overflow_one(),
            FauxResponseStep::Message(assistant("lane retry succeeded", StopReason::Stop, None)),
        ]);
        let stream_core = core.clone();
        let stream_fn: StreamFn = Arc::new(move |request_model, context| {
            stream_core.stream(request_model, context, None)
        });
        let recovery_calls = Arc::new(AtomicUsize::new(0));
        let recovery_calls_for_hook = recovery_calls.clone();
        let recovery: OverflowRecoveryHook = Arc::new(move |request, _signal| {
            recovery_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            let mut context = request.context;
            context.messages = request.retry_messages;
            Box::pin(async move { Ok(OverflowRecoveryResult { context }) })
        });
        let (harness_session, _hook_session) = shared_session("harness-lane-overflow");
        let mut options = AgentHarnessOptions::new(harness_session, model());
        options.stream_fn = Some(stream_fn);
        options.overflow_recovery = Some(recovery);
        let (harness, suspended) = AgentHarness::create(options)
            .await
            .expect("harness should create");
        assert!(suspended.is_empty());

        let lane = harness
            .create_lane("worker", None)
            .await
            .expect("lane should create");
        lane.prompt_messages(&[pi_agent::user_text_prompt("recover in worker", 1)])
            .await
            .expect("lane run should settle");

        assert_eq!(recovery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(core.get_pending_response_count(), 0);
    });
}
