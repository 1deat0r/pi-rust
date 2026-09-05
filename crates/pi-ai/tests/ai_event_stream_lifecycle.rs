#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::event_stream::{AssistantMessageEventStream, StreamSink, StreamSinkAdapter};
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason, ErrorReason, StopReason,
    Usage,
};
use serde_json::json;

fn message(reason: StopReason) -> AssistantMessage {
    let mut message = AssistantMessage::new().with_timestamp(1);
    message.set_stop_reason(reason);
    message
}

#[tokio::test]
async fn provider_shaped_success_preserves_the_complete_lifecycle_in_order() {
    let mut stream = AssistantMessageEventStream::new();
    let partial = AssistantMessage::new().with_timestamp(1);
    let tool_call = ContentBlock::tool_call("call-1", "read", json!({"path":"README.md"}));
    let mut done = message(StopReason::ToolUse);
    done.set_content(vec![
        ContentBlock::thinking("inspect"),
        ContentBlock::text("using a tool"),
        tool_call.clone(),
    ]);
    done.set_usage(Usage {
        input: 7,
        output: 5,
        total_tokens: 12,
        ..Usage::default()
    });

    stream.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::ThinkingStart {
        content_index: 0,
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::ThinkingDelta {
        content_index: 0,
        delta: "inspect".into(),
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::ThinkingEnd {
        content_index: 0,
        content: "inspect".into(),
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::TextStart {
        content_index: 1,
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::TextDelta {
        content_index: 1,
        delta: "using a tool".into(),
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::TextEnd {
        content_index: 1,
        content: "using a tool".into(),
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::ToolCallStart {
        content_index: 2,
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::ToolCallDelta {
        content_index: 2,
        delta: r#"{"path":"README.md"}"#.into(),
        partial: partial.clone(),
    });
    stream.push(AssistantMessageEvent::ToolCallEnd {
        content_index: 2,
        tool_call,
        partial,
    });
    stream.push(AssistantMessageEvent::Done {
        reason: DoneReason::ToolUse,
        message: done.clone(),
    });

    let (events, result) = stream.collect().await;
    assert_eq!(events.len(), 11);
    assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
    assert!(matches!(
        events[1],
        AssistantMessageEvent::ThinkingStart { .. }
    ));
    assert!(matches!(
        events[2],
        AssistantMessageEvent::ThinkingDelta { .. }
    ));
    assert!(matches!(
        events[3],
        AssistantMessageEvent::ThinkingEnd { .. }
    ));
    assert!(matches!(events[4], AssistantMessageEvent::TextStart { .. }));
    assert!(matches!(events[5], AssistantMessageEvent::TextDelta { .. }));
    assert!(matches!(events[6], AssistantMessageEvent::TextEnd { .. }));
    assert!(matches!(
        events[7],
        AssistantMessageEvent::ToolCallStart { .. }
    ));
    assert!(matches!(
        events[8],
        AssistantMessageEvent::ToolCallDelta { .. }
    ));
    assert!(matches!(
        events[9],
        AssistantMessageEvent::ToolCallEnd { .. }
    ));
    assert!(matches!(events[10], AssistantMessageEvent::Done { .. }));
    assert_eq!(result, done);
    assert_eq!(result.usage().map(|usage| usage.total_tokens), Some(12));
}

#[tokio::test]
async fn asynchronous_end_publishes_its_result_before_waking_the_consumer() {
    for iteration in 0..1_000_u64 {
        let stream = AssistantMessageEventStream::new();
        let sender = stream.sender().expect("sender");
        let end = stream.end_handle();
        let expected = message(StopReason::Length).with_timestamp(iteration + 1);
        let producer_result = expected.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let mut sink = StreamSinkAdapter::new_with_end(sender, end);
            sink.end(Some(producer_result));
        });

        let (_, result) = tokio::time::timeout(Duration::from_secs(1), stream.collect())
            .await
            .expect("asynchronous completion must settle");
        assert_eq!(result, expected);
    }
}

#[tokio::test]
async fn abort_error_is_terminal_exactly_once_and_observed_once() {
    let mut stream = AssistantMessageEventStream::new();
    let aborted = message(StopReason::Aborted);
    stream.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error_message: aborted.clone(),
    });
    stream.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: message(StopReason::Stop),
    });

    let observed = Arc::new(Mutex::new(Vec::new()));
    let capture = Arc::clone(&observed);
    let result = stream
        .for_each(move |event| capture.lock().unwrap().push(event))
        .await;
    let events = observed.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            ..
        }
    ));
    assert_eq!(result, aborted);
}

#[tokio::test]
async fn abort_after_terminal_success_is_ignored_without_duplicate_settlement() {
    let mut stream = AssistantMessageEventStream::new();
    let done = message(StopReason::Stop);
    stream.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: done.clone(),
    });
    stream.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error_message: message(StopReason::Aborted),
    });

    let (events, result) = stream.collect().await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], AssistantMessageEvent::Done { .. }));
    assert_eq!(result, done);
}

#[tokio::test]
async fn consumer_drop_closes_the_sender_and_failed_delivery_is_non_blocking() {
    let stream = AssistantMessageEventStream::new();
    let sender = stream.sender().expect("sender");
    drop(stream);

    assert!(sender
        .send(AssistantMessageEvent::Start {
            partial: AssistantMessage::new(),
        })
        .is_err());
}
