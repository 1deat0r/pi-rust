//! `AssistantMessageEventStream` — port of `packages/ai/src/utils/event-stream.ts`.
//!
//! A push-based stream of `AssistantMessageEvent`s that terminates with a
//! final `AssistantMessage` (`done` or `error`). Providers push events while
//! consumers iterate or await the final result.

use tokio::sync::mpsc;

use crate::types::{AssistantMessage, AssistantMessageEvent, ErrorReason};

/// Push surface shared by consumer-side streams and background producers.
pub trait StreamSink {
    fn push(&mut self, event: AssistantMessageEvent);
    fn end(&mut self, result: Option<AssistantMessage>);
}

pub struct AssistantMessageEventStream {
    tx: Option<mpsc::UnboundedSender<AssistantMessageEvent>>,
    rx: mpsc::UnboundedReceiver<AssistantMessageEvent>,
    /// Final result delivered on completion.
    result: Option<tokio::sync::oneshot::Sender<AssistantMessage>>,
    finished: bool,
}

impl StreamSink for AssistantMessageEventStream {
    fn push(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }) {
            self.finished = true;
            let message = match &event {
                AssistantMessageEvent::Done { message, .. } => message.clone(),
                AssistantMessageEvent::Error { error_message, .. } => error_message.clone(),
                _ => unreachable!(),
            };
            if let Some(tx) = self.result.take() {
                let _ = tx.send(message);
            }
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
    fn end(&mut self, result: Option<AssistantMessage>) {
        self.finished = true;
        if let Some(message) = result {
            if let Some(tx) = self.result.take() {
                let _ = tx.send(message);
            }
        }
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
        let (result_tx, _) = tokio::sync::oneshot::channel();
        Self { tx: Some(tx), rx, result: Some(result_tx), finished: false }
    }

    /// Clone access to the underlying event channel, for background producers.
    /// Returns None after the stream has ended (channel closed).
    pub fn sender(&self) -> Option<mpsc::UnboundedSender<AssistantMessageEvent>> {
        self.tx.clone()
    }

    /// Drain events while forwarding each to `observer`, then return the
    /// final message (used by RPC-mode streaming: the agent loop observes
    /// every `AssistantMessageEvent` while still awaiting the result).
    pub async fn collect_with_observer(
        mut self,
        observer: &std::sync::Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
    ) -> AssistantMessage {
        let mut final_message: Option<AssistantMessage> = None;
        while let Some(event) = self.rx.recv().await {
            match &event {
                AssistantMessageEvent::Done { message, .. } | AssistantMessageEvent::Error { error_message: message, .. } => {
                    final_message = Some(message.clone());
                }
                _ => {}
            }
            observer(&event);
            if matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }) {
                break;
            }
        }
        final_message.unwrap_or_else(AssistantMessage::new)
    }

    /// Push an event. Drops after completion (mirrors upstream `if (this.done) return`).
    pub fn push(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }) {
            self.finished = true;
            let message = match &event {
                AssistantMessageEvent::Done { message, .. } => message.clone(),
                AssistantMessageEvent::Error { error_message, .. } => error_message.clone(),
                _ => unreachable!(),
            };
            if let Some(tx) = self.result.take() {
                let _ = tx.send(message);
            }
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
        if let Some(message) = result {
            if let Some(tx) = self.result.take() {
                let _ = tx.send(message);
            }
        }
        // Dropping our own sender closes the channel for consumers that hold
        // only the receiver; background producers still hold clones until they
        // terminate, after which the channel closes too.
        self.tx = None;
    }

    /// Consume all events until completion, returning the final message.
    pub async fn collect(mut self) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
        let mut events = Vec::new();
        while let Some(event) = self.rx.recv().await {
            let is_final = matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. });
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
            .unwrap_or_default();
        (events, final_message)
    }

    /// Iterate events by callback until the terminal event, returning the final message.
    pub async fn for_each<F>(mut self, mut f: F) -> AssistantMessage
    where
        F: FnMut(AssistantMessageEvent),
    {
        let mut final_message = AssistantMessage::new();
        while let Some(event) = self.rx.recv().await {
            let terminal = matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. });
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
        final_message
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DoneReason;
    use crate::types::ContentBlock;

    #[test]
    fn push_and_collect() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut stream = AssistantMessageEventStream::new();
            let mut partial = AssistantMessage::new();
            stream.push(AssistantMessageEvent::Start { partial: partial.clone() });
            let block = ContentBlock::text("hi");
            partial.content_mut().push(block);
            let mut done = AssistantMessage::new();
            done.content_mut().push(ContentBlock::text("hi"));
            done.set_stop_reason(crate::types::StopReason::Stop);
            stream.push(AssistantMessageEvent::Done { reason: DoneReason::Stop, message: done.clone() });
            let (events, final_msg) = stream.collect().await;
            assert_eq!(events.len(), 2);
            assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
            assert!(matches!(events[1], AssistantMessageEvent::Done { .. }));
            assert_eq!(final_msg.stop_reason(), Some(crate::types::StopReason::Stop));
        });
    }

    #[test]
    fn end_without_terminal_event_terminates_consumers() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEvent::Start { partial: AssistantMessage::new() });
            stream.end(None);
            let (events, msg) = stream.collect().await;
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
            assert_eq!(msg.stop_reason(), None);
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
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut stream = AssistantMessageEventStream::new();
            let mut done = AssistantMessage::new();
            done.set_stop_reason(crate::types::StopReason::Stop);
            stream.push(AssistantMessageEvent::Done { reason: DoneReason::Stop, message: done });
            stream.push(AssistantMessageEvent::Start { partial: AssistantMessage::new() });
            let (events, _) = stream.collect().await;
            assert_eq!(events.len(), 1);
        });
    }
}


/// Adapter that wraps a raw `UnboundedSender` clone in the `StreamSink`
/// surface (used by providers that push from spawned tasks).
pub struct StreamSinkAdapter {
    tx: mpsc::UnboundedSender<AssistantMessageEvent>,
    finished: bool,
}

impl StreamSinkAdapter {
    pub fn new(tx: mpsc::UnboundedSender<AssistantMessageEvent>) -> Self {
        Self { tx, finished: false }
    }
}

impl StreamSink for StreamSinkAdapter {
    fn push(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }) {
            self.finished = true;
        }
        let _ = self.tx.send(event);
    }
    fn end(&mut self, _result: Option<AssistantMessage>) {
        self.finished = true;
    }
}
