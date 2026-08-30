#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Production seam tests for the shared rich-agent overflow path.
//!
//! The recovery callback in these tests stands in for the caller's real
//! durable session/compaction implementation. Provider requests still travel
//! through the normal pi-ai stream boundary; the test only supplies a
//! deterministic provider response sequence.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::agent::{AgentContext, StreamFn};
use pi_agent::rich_agent::{
    run_rich_agent_loop, Agent, OverflowRecoveryHook, OverflowRecoveryReason,
    OverflowRecoveryResult, RichAgentEvent, RichAgentLoopConfig,
};
use pi_agent::types::{AgentMessage, CustomAgentMessage};
use pi_ai::providers::{
    faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pi_ai::types::{AssistantMessage, ContentBlock, Message, StopReason, Usage};

type RecoveryRequestSnapshot = (
    OverflowRecoveryReason,
    bool,
    Vec<AgentMessage>,
    Vec<AgentMessage>,
);
type RecoveryRequests = Arc<Mutex<Vec<RecoveryRequestSnapshot>>>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

fn prompt(text: &str) -> AgentMessage {
    pi_agent::user_text_prompt(text, 1)
}

fn assistant(
    stop_reason: StopReason,
    error_message: Option<&str>,
    usage: Option<Usage>,
) -> AssistantMessage {
    let mut message = faux_assistant_message(
        vec![ContentBlock::text("provider response")],
        FauxAssistantOptions {
            stop_reason: Some(stop_reason),
            error_message: error_message.map(str::to_string),
        },
    );
    if let Some(usage) = usage {
        message.set_usage(usage);
    }
    message
}

fn usage(input: i64, output: i64) -> Usage {
    Usage {
        input,
        output,
        total_tokens: input.saturating_add(output),
        ..Default::default()
    }
}

fn recovery_hook(calls: Arc<AtomicUsize>, requests: RecoveryRequests) -> OverflowRecoveryHook {
    Arc::new(move |request, _signal| {
        calls.fetch_add(1, Ordering::SeqCst);
        requests.lock().expect("request lock").push((
            request.reason,
            request.will_retry,
            request.durable_messages.clone(),
            request.retry_messages.clone(),
        ));

        let mut context = request.context;
        context.messages = request.retry_messages;
        context.messages.push(AgentMessage::Custom(
            CustomAgentMessage::CompactionSummary {
                summary: "real compaction seam summary".to_string(),
                tokens_before: 100,
                timestamp: 2,
            },
        ));
        Box::pin(async move { Ok(OverflowRecoveryResult { context }) })
    })
}

fn response_has_error(message: &AgentMessage, expected: &str) -> bool {
    matches!(
        message,
        AgentMessage::Core(Message::Assistant(response))
            if response.stop_reason() == Some(StopReason::Error)
                && response.error_message() == Some(expected)
    )
}

fn response_has_text(message: &AgentMessage, expected: &str) -> bool {
    matches!(
        message,
        AgentMessage::Core(Message::Assistant(response))
            if response.content().iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text == expected)
            })
    )
}

#[test]
fn provider_overflow_compacts_once_and_retries_without_failed_response() {
    runtime().block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        core.set_responses(vec![
            FauxResponseStep::Message(assistant(
                StopReason::Error,
                Some("prompt is too long for the model context window"),
                Some(usage(100, 0)),
            )),
            FauxResponseStep::Message(assistant(StopReason::Stop, None, Some(usage(20, 2)))),
        ]);
        let model = core.get_model(None).expect("faux model").clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen_contexts = Arc::new(Mutex::new(Vec::<Vec<Message>>::new()));
        let stream_core = core.clone();
        let seen_for_stream = seen_contexts.clone();
        let provider_calls_for_stream = provider_calls.clone();
        let stream_fn: StreamFn = Arc::new(move |model, context| {
            provider_calls_for_stream.fetch_add(1, Ordering::SeqCst);
            seen_for_stream
                .lock()
                .expect("context lock")
                .push(context.messages.clone());
            stream_core.stream(model, context, None)
        });

        let mut config = RichAgentLoopConfig::new(model, stream_fn, None);
        config.overflow_recovery = Some(recovery_hook(calls.clone(), requests.clone()));
        let mut context = AgentContext::new(None, Vec::new());
        let mut events = Vec::new();
        let returned = run_rich_agent_loop(
            vec![prompt("recover this")],
            &mut context,
            &config,
            &mut |event| events.push(event),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        let requests = requests.lock().expect("request lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, OverflowRecoveryReason::ContextOverflow);
        assert!(requests[0].1);
        assert!(requests[0]
            .2
            .iter()
            .any(|message| response_has_error(message, "prompt is too long for the model context window")));
        assert!(!requests[0]
            .3
            .iter()
            .any(|message| response_has_error(message, "prompt is too long for the model context window")));

        let seen_contexts = seen_contexts.lock().expect("context lock");
        assert_eq!(seen_contexts.len(), 2);
        assert!(!seen_contexts[1].iter().any(|message| {
            matches!(
                message,
                Message::Assistant(response)
                    if response.error_message() == Some("prompt is too long for the model context window")
            )
        }));
        assert!(context.messages.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Custom(CustomAgentMessage::CompactionSummary { summary, .. })
                    if summary == "real compaction seam summary"
            )
        }));
        assert!(!context
            .messages
            .iter()
            .any(|message| response_has_error(message, "prompt is too long for the model context window")));
        assert!(returned
            .iter()
            .any(|message| response_has_error(message, "prompt is too long for the model context window")));
        assert!(returned
            .iter()
            .any(|message| response_has_text(message, "provider response")));
        assert!(events.iter().any(|event| matches!(
            event,
            RichAgentEvent::TurnEnd {
                message: AgentMessage::Core(Message::Assistant(response)),
                ..
            } if response.error_message() == Some("prompt is too long for the model context window")
        )));
    });
}

#[test]
fn recoverable_length_uses_the_same_bounded_compact_and_retry_path() {
    runtime().block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        core.set_responses(vec![
            FauxResponseStep::Message(assistant(StopReason::Length, None, Some(usage(20, 4)))),
            FauxResponseStep::Message(assistant(StopReason::Stop, None, Some(usage(20, 2)))),
        ]);
        let mut model = core.get_model(None).expect("faux model").clone();
        model.max_tokens = 8;
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut config = RichAgentLoopConfig::new(
            model,
            Arc::new(move |model, context| core.stream(model, context, None)),
            None,
        );
        config.overflow_recovery = Some(recovery_hook(calls.clone(), requests.clone()));
        let mut context = AgentContext::new(None, Vec::new());
        let returned = run_rich_agent_loop(
            vec![prompt("recover truncated output")],
            &mut context,
            &config,
            &mut |_| {},
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let requests = requests.lock().expect("request lock");
        assert_eq!(requests[0].0, OverflowRecoveryReason::RecoverableLength);
        assert!(requests[0].1);
        assert!(returned.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Core(Message::Assistant(response))
                    if response.stop_reason() == Some(StopReason::Length)
            )
        }));
        assert!(returned
            .iter()
            .any(|message| response_has_text(message, "provider response")));
        assert!(!context.messages.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Core(Message::Assistant(response))
                    if response.stop_reason() == Some(StopReason::Length)
            )
        }));
    });
}

#[test]
fn second_overflow_is_terminal_after_one_recovery_attempt() {
    runtime().block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        core.set_responses(vec![
            FauxResponseStep::Message(assistant(
                StopReason::Error,
                Some("request_too_large"),
                Some(usage(100, 0)),
            )),
            FauxResponseStep::Message(assistant(
                StopReason::Error,
                Some("exceeds the context window"),
                Some(usage(100, 0)),
            )),
        ]);
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stream_core = core.clone();
        let provider_calls_for_stream = provider_calls.clone();
        let mut config = RichAgentLoopConfig::new(
            core.get_model(None).expect("faux model").clone(),
            Arc::new(move |model, context| {
                provider_calls_for_stream.fetch_add(1, Ordering::SeqCst);
                stream_core.stream(model, context, None)
            }),
            None,
        );
        config.overflow_recovery = Some(recovery_hook(calls.clone(), requests));
        let mut context = AgentContext::new(None, Vec::new());
        let returned = run_rich_agent_loop(
            vec![prompt("do not loop")],
            &mut context,
            &config,
            &mut |_| {},
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        assert!(returned
            .iter()
            .any(|message| response_has_error(message, "request_too_large")));
        assert!(returned
            .iter()
            .any(|message| response_has_error(message, "exceeds the context window")));
        assert!(context
            .messages
            .iter()
            .any(|message| response_has_error(message, "exceeds the context window")));
    });
}

#[test]
fn successful_overflow_compacts_without_retry_and_preserves_response() {
    runtime().block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        let mut overflow = assistant(StopReason::Stop, None, Some(usage(101, 1)));
        overflow.set_content(vec![ContentBlock::text("completed despite window")]);
        core.set_responses(vec![FauxResponseStep::Message(overflow)]);
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let mut model = core.get_model(None).expect("faux model").clone();
        model.context_window = 100;
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider_calls_for_stream = provider_calls.clone();
        let mut config = RichAgentLoopConfig::new(
            model,
            Arc::new(move |model, context| {
                provider_calls_for_stream.fetch_add(1, Ordering::SeqCst);
                core.stream(model, context, None)
            }),
            None,
        );
        config.overflow_recovery = Some(recovery_hook(calls.clone(), requests.clone()));
        let mut context = AgentContext::new(None, Vec::new());
        // The faux provider computes usage from the actual request context,
        // so make this deterministic successful response exceed the tiny
        // configured window through real request accounting.
        context.messages.push(prompt(&"x".repeat(500)));
        let returned = run_rich_agent_loop(
            vec![prompt("preserve completed response")],
            &mut context,
            &config,
            &mut |_| {},
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
        let requests = requests.lock().expect("request lock");
        assert!(!requests[0].1);
        assert!(context
            .messages
            .iter()
            .any(|message| response_has_text(message, "completed despite window")));
        assert!(returned
            .iter()
            .any(|message| response_has_text(message, "completed despite window")));
    });
}

#[test]
fn stateful_agent_keeps_failed_response_durable_but_not_active() {
    runtime().block_on(async {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
        core.set_responses(vec![
            FauxResponseStep::Message(assistant(
                StopReason::Error,
                Some("prompt too long; exceeded max context length"),
                Some(usage(100, 0)),
            )),
            FauxResponseStep::Message(assistant(StopReason::Stop, None, Some(usage(20, 2)))),
        ]);
        let model = core.get_model(None).expect("faux model").clone();
        let mut agent = Agent::new(Arc::new(move |model, context| {
            core.stream(model, context, None)
        }));
        {
            let mut state = agent.state();
            state.model = model;
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        agent.set_overflow_recovery(recovery_hook(calls.clone(), requests));
        let durable_events = Arc::new(Mutex::new(Vec::<AgentMessage>::new()));
        let durable_events_for_listener = durable_events.clone();
        let _unsubscribe = agent.subscribe(move |event, _signal| {
            if let RichAgentEvent::MessageEnd { message } = event {
                durable_events_for_listener
                    .lock()
                    .expect("event lock")
                    .push(message);
            }
            Box::pin(async {})
        });

        let durable_delta = agent
            .prompt_messages(vec![prompt("stateful recovery")])
            .await
            .expect("prompt should settle");
        let active_messages = agent.state().messages().to_vec();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(durable_delta.iter().any(|message| {
            response_has_error(message, "prompt too long; exceeded max context length")
        }));
        assert!(durable_events
            .lock()
            .expect("event lock")
            .iter()
            .any(|message| {
                response_has_error(message, "prompt too long; exceeded max context length")
            }));
        assert!(!active_messages.iter().any(|message| {
            response_has_error(message, "prompt too long; exceeded max context length")
        }));
        assert!(active_messages.iter().any(|message| {
            matches!(
                message,
                AgentMessage::Custom(CustomAgentMessage::CompactionSummary { summary, .. })
                    if summary == "real compaction seam summary"
            )
        }));
    });
}
