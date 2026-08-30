//! `AssistantMessageEventStream` — port of `packages/ai/src/utils/event-stream.ts`.
//!
//! A push-based stream of `AssistantMessageEvent`s that terminates with a
//! final `AssistantMessage` (`done` or `error`). Providers push events while
//! consumers iterate or await the final result.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use tokio::sync::{mpsc, Notify};

use crate::types::{AssistantMessage, AssistantMessageEvent, ErrorReason};

/// Push surface shared by consumer-side streams and background producers.
pub trait StreamSink {
    fn push(&mut self, event: AssistantMessageEvent);
    fn end(&mut self, result: Option<AssistantMessage>);
}

struct StreamEndState {
    ended: AtomicBool,
    notify: Notify,
    result: Mutex<Option<AssistantMessage>>,
}

/// Completion handle shared with a producer that only owns a sender clone.
/// Dropping that clone cannot close the stream while the stream itself still
/// owns its sender, so adapters use this handle to reproduce `EventStream.end`.
#[derive(Clone)]
pub struct StreamEndHandle(Arc<StreamEndState>);

impl StreamEndHandle {
    fn finish(&self, result: Option<AssistantMessage>) {
        if self.0.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(message) = result {
            *self
                .0
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(message);
        }
        self.0.notify.notify_waiters();
    }

    fn result(&self) -> Option<AssistantMessage> {
        self.0
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

pub struct AssistantMessageEventStream {
    tx: Option<mpsc::UnboundedSender<AssistantMessageEvent>>,
    rx: mpsc::UnboundedReceiver<AssistantMessageEvent>,
    end_state: StreamEndHandle,
    finished: bool,
}

impl StreamSink for AssistantMessageEventStream {
    fn push(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        ) {
            self.finished = true;
            let message = match &event {
                AssistantMessageEvent::Done { message, .. } => message.clone(),
                AssistantMessageEvent::Error { error_message, .. } => error_message.clone(),
                _ => unreachable!(),
            };
            if let Some(tx) = &self.tx {
                let _ = tx.send(event);
            }
            self.end_state.finish(Some(message));
        } else if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
    fn end(&mut self, result: Option<AssistantMessage>) {
        self.finished = true;
        self.end_state.finish(result);
        self.tx = None;
    }
}

impl Default for AssistantMessageEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantMessageEventStream {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let end_state = StreamEndHandle(Arc::new(StreamEndState {
            ended: AtomicBool::new(false),
            notify: Notify::new(),
            result: Mutex::new(None),
        }));
        Self {
            tx: Some(tx),
            rx,
            end_state,
            finished: false,
        }
    }

    /// Clone access to the underlying event channel, for background producers.
    /// Returns None after the stream has ended (channel closed).
    pub fn sender(&self) -> Option<mpsc::UnboundedSender<AssistantMessageEvent>> {
        self.tx.clone()
    }

    /// Return a completion handle for a producer that forwards through a
    /// sender clone. The handle is separate from `sender()` so existing raw
    /// producers retain their channel-only API.
    pub fn end_handle(&self) -> StreamEndHandle {
        self.end_state.clone()
    }

    async fn recv_event(&mut self) -> Option<AssistantMessageEvent> {
        loop {
            if self.end_state.0.ended.load(Ordering::SeqCst) && self.rx.is_empty() {
                return None;
            }
            tokio::select! {
                biased;
                event = self.rx.recv() => return event,
                _ = self.end_state.0.notify.notified() => {},
            }
        }
    }

    /// Drain events while forwarding each to `observer`, then return the
    /// final message (used by RPC-mode streaming: the agent loop observes
    /// every `AssistantMessageEvent` while still awaiting the result).
    pub async fn collect_with_observer(
        mut self,
        observer: &std::sync::Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
    ) -> AssistantMessage {
        let mut final_message: Option<AssistantMessage> = None;
        while let Some(event) = self.recv_event().await {
            match &event {
                AssistantMessageEvent::Done { message, .. }
                | AssistantMessageEvent::Error {
                    error_message: message,
                    ..
                } => {
                    final_message = Some(message.clone());
                }
                _ => {}
            }
            observer(&event);
            if matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ) {
                break;
            }
        }
        final_message
            .or_else(|| self.end_state.result())
            .unwrap_or_default()
    }

    /// Push an event. Drops after completion (mirrors upstream `if (this.done) return`).
    pub fn push(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        ) {
            self.finished = true;
            let message = match &event {
                AssistantMessageEvent::Done { message, .. } => message.clone(),
                AssistantMessageEvent::Error { error_message, .. } => error_message.clone(),
                _ => unreachable!(),
            };
            self.end_state.finish(Some(message));
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    /// Complete the stream with an optional result without a terminal event.
    /// Closing the channel terminates consumers (`collect`/`for_each`) on
    /// exhaustion, matching upstream `EventStream.end()` which wakes waiting
    /// consumers with `done: true`.
    pub fn end(&mut self, result: Option<AssistantMessage>) {
        self.finished = true;
        self.end_state.finish(result);
        // Dropping our own sender closes the channel for consumers that hold
        // only the receiver; background producers still hold clones until they
        // terminate, after which the channel closes too.
        self.tx = None;
    }

    /// Consume all events until completion, returning the final message.
    pub async fn collect(mut self) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
        let mut events = Vec::new();
        while let Some(event) = self.recv_event().await {
            let is_final = matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            );
            events.push(event);
            if is_final {
                break;
            }
        }
        let final_message = events
            .iter()
            .rev()
            .find_map(|e| match e {
                AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
                AssistantMessageEvent::Error { error_message, .. } => Some(error_message.clone()),
                _ => None,
            })
            .or_else(|| self.end_state.result())
            .unwrap_or_default();
        (events, final_message)
    }

    /// Iterate events by callback until the terminal event, returning the final message.
    pub async fn for_each<F>(mut self, mut f: F) -> AssistantMessage
    where
        F: FnMut(AssistantMessageEvent),
    {
        let mut final_message = AssistantMessage::new();
        while let Some(event) = self.recv_event().await {
            let terminal = matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            );
            if let AssistantMessageEvent::Done { message, .. } = &event {
                final_message = message.clone();
            } else if let AssistantMessageEvent::Error { error_message, .. } = &event {
                final_message = error_message.clone();
            }
            f(event);
            if terminal {
                break;
            }
        }
        if final_message.stop_reason().is_none() {
            self.end_state.result().unwrap_or(final_message)
        } else {
            final_message
        }
    }
}

/// Create a stream that immediately emits an error event with the given
/// message (used by providers when request execution fails).
pub fn create_error_stream(
    api: &str,
    provider: &str,
    model: &str,
    message: String,
) -> AssistantMessageEventStream {
    let mut stream = AssistantMessageEventStream::new();
    let mut msg = AssistantMessage::new();
    msg.set_api_provider_model(api, provider, model);
    msg.set_stop_reason(crate::types::StopReason::Error);
    let AssistantMessage::Assistant { error_message, .. } = &mut msg;
    *error_message = Some(message);
    stream.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Error,
        error_message: msg.clone(),
    });
    stream
}

/// Adapter that wraps a raw `UnboundedSender` clone in the `StreamSink`
/// surface (used by providers that push from spawned tasks).
pub struct StreamSinkAdapter {
    tx: mpsc::UnboundedSender<AssistantMessageEvent>,
    end_state: Option<StreamEndHandle>,
    finished: bool,
}

impl StreamSinkAdapter {
    pub fn new(tx: mpsc::UnboundedSender<AssistantMessageEvent>) -> Self {
        Self {
            tx,
            end_state: None,
            finished: false,
        }
    }

    pub fn new_with_end(
        tx: mpsc::UnboundedSender<AssistantMessageEvent>,
        end_state: StreamEndHandle,
    ) -> Self {
        Self {
            tx,
            end_state: Some(end_state),
            finished: false,
        }
    }
}

impl StreamSink for StreamSinkAdapter {
    fn push(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        let terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        if terminal {
            self.finished = true;
        }
        let message = terminal.then(|| match &event {
            AssistantMessageEvent::Done { message, .. } => message.clone(),
            AssistantMessageEvent::Error { error_message, .. } => error_message.clone(),
            _ => unreachable!(),
        });
        let _ = self.tx.send(event);
        if let Some(message) = message {
            if let Some(end_state) = &self.end_state {
                end_state.finish(Some(message));
            }
        }
    }
    fn end(&mut self, result: Option<AssistantMessage>) {
        self.finished = true;
        if let Some(end_state) = &self.end_state {
            end_state.finish(result);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;
    use crate::types::DoneReason;

    #[test]
    fn push_and_collect() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut stream = AssistantMessageEventStream::new();
            let mut partial = AssistantMessage::new();
            stream.push(AssistantMessageEvent::Start {
                partial: partial.clone(),
            });
            let block = ContentBlock::text("hi");
            partial.content_mut().push(block);
            let mut done = AssistantMessage::new();
            done.content_mut().push(ContentBlock::text("hi"));
            done.set_stop_reason(crate::types::StopReason::Stop);
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: done.clone(),
            });
            let (events, final_msg) = stream.collect().await;
            assert_eq!(events.len(), 2);
            assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
            assert!(matches!(events[1], AssistantMessageEvent::Done { .. }));
            assert_eq!(
                final_msg.stop_reason(),
                Some(crate::types::StopReason::Stop)
            );
        });
    }

    #[test]
    fn end_without_terminal_event_terminates_consumers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEvent::Start {
                partial: AssistantMessage::new(),
            });
            stream.end(None);
            let (events, msg) = stream.collect().await;
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
            assert_eq!(msg.stop_reason(), None);
        });
    }

    #[test]
    fn adapter_end_without_terminal_event_wakes_consumers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let stream = AssistantMessageEventStream::new();
            let tx = stream.sender().unwrap();
            let end_handle = stream.end_handle();
            tokio::spawn(async move {
                let mut sink = StreamSinkAdapter::new_with_end(tx, end_handle);
                sink.push(AssistantMessageEvent::Start {
                    partial: AssistantMessage::new(),
                });
                sink.end(None);
            });

            let (events, message) =
                tokio::time::timeout(std::time::Duration::from_secs(1), stream.collect())
                    .await
                    .expect("adapter end must settle the stream");
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
            assert_eq!(message.stop_reason(), None);
        });
    }

    #[test]
    fn adapter_end_preserves_result_and_settles_only_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let stream = AssistantMessageEventStream::new();
            let tx = stream.sender().unwrap();
            let end_handle = stream.end_handle();
            let mut first = AssistantMessage::new();
            first.set_stop_reason(crate::types::StopReason::Stop);
            let mut second = AssistantMessage::new();
            second.set_stop_reason(crate::types::StopReason::Error);
            tokio::spawn(async move {
                let mut sink = StreamSinkAdapter::new_with_end(tx, end_handle);
                sink.end(Some(first));
                sink.end(Some(second));
            });

            let (_, message) =
                tokio::time::timeout(std::time::Duration::from_secs(1), stream.collect())
                    .await
                    .expect("adapter end must settle the stream");
            assert_eq!(message.stop_reason(), Some(crate::types::StopReason::Stop));
        });
    }

    #[test]
    fn adapter_terminal_event_is_not_lost_when_producer_is_async() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let stream = AssistantMessageEventStream::new();
            let tx = stream.sender().unwrap();
            let end_handle = stream.end_handle();
            tokio::spawn(async move {
                let mut sink = StreamSinkAdapter::new_with_end(tx, end_handle);
                let mut message = AssistantMessage::new();
                message.set_stop_reason(crate::types::StopReason::Stop);
                sink.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message,
                });
            });

            let (events, message) =
                tokio::time::timeout(std::time::Duration::from_secs(1), stream.collect())
                    .await
                    .expect("terminal adapter event must settle the stream");
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0], AssistantMessageEvent::Done { .. }));
            assert_eq!(message.stop_reason(), Some(crate::types::StopReason::Stop));
        });
    }

    #[test]
    fn sender_is_none_after_end() {
        let mut stream = AssistantMessageEventStream::new();
        assert!(stream.sender().is_some());
        stream.end(None);
        assert!(stream.sender().is_none());
    }

    #[test]
    fn ignores_push_after_done() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut stream = AssistantMessageEventStream::new();
            let mut done = AssistantMessage::new();
            done.set_stop_reason(crate::types::StopReason::Stop);
            stream.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: done,
            });
            stream.push(AssistantMessageEvent::Start {
                partial: AssistantMessage::new(),
            });
            let (events, _) = stream.collect().await;
            assert_eq!(events.len(), 1);
        });
    }
}
