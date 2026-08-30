//! Remote coding-agent session facade.
//!
//! This is the Rust counterpart of the upstream remote-session wrapper.  It
//! deliberately keeps the transport/client boundary separate from the local
//! interactive runtime: snapshots and incremental transcript progress are
//! reduced into one immutable state view, while commands are lease-backed and
//! serialized per remote session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_client::{AcquireSessionOptions, PiClient, PiClientError, SessionHandle, SessionLeaseMode};
use pi_protocol::{
    AssistantContent, ModelRef, ServerEvent, SessionMetadata, SessionPhase, SessionSnapshot,
    TextContent, ThinkingContent, ToolCallContent, TranscriptDeltaKind, TranscriptItem,
    TranscriptItemFinished, TranscriptItemUpdate, TranscriptProgress,
};
#[cfg(test)]
use pi_protocol::{AssistantStatus, AssistantTranscriptItem};
use serde_json::Value as JsonValue;
use thiserror::Error;

pub type RemoteSessionListener = Arc<dyn Fn(RemoteSessionState) + Send + Sync>;
pub type RemoteSessionUnsubscribe = Box<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteSessionOperation {
    Open,
    Create,
    Submit,
    Abort,
    SetModel,
    SetThinking,
    Reconnect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSessionLifecycle {
    Unbound,
    Ready,
    Busy(RemoteSessionOperation),
    Disposed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteSessionState {
    pub lifecycle: RemoteSessionLifecycle,
    pub snapshot: Option<SessionSnapshot>,
    pub transcript: Vec<TranscriptItem>,
}

#[derive(Debug, Clone)]
pub struct CreateRemoteSessionOptions {
    pub cwd: String,
    pub model: Option<ModelRef>,
    pub thinking_level: Option<pi_protocol::ThinkingLevel>,
}

#[derive(Default)]
pub struct RemoteSessionOptions {
    pub on_listener_error: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteSessionError {
    #[error("Remote session is disposed")]
    Disposed,
    #[error("No remote session is attached")]
    Unbound,
    #[error("Remote session is busy with {0:?}")]
    Busy(RemoteSessionOperation),
    #[error("Session cannot accept input during {0}")]
    InvalidPhase(String),
    #[error("Cannot {operation} while session is {phase}")]
    InvalidOperation { operation: String, phase: String },
    #[error("Remote session client error: {0}")]
    Client(String),
}

impl From<PiClientError> for RemoteSessionError {
    fn from(value: PiClientError) -> Self {
        Self::Client(value.to_string())
    }
}

#[derive(Debug, Clone)]
struct TranscriptState {
    snapshot: SessionSnapshot,
    progress_items: HashMap<String, TranscriptItem>,
    progress_order: Vec<String>,
    tool_call_buffers: HashMap<String, String>,
}

impl TranscriptState {
    fn new(snapshot: SessionSnapshot) -> Self {
        Self {
            snapshot,
            progress_items: HashMap::new(),
            progress_order: Vec::new(),
            tool_call_buffers: HashMap::new(),
        }
    }

    fn selected(&self) -> Vec<TranscriptItem> {
        let mut result = self
            .snapshot
            .transcript
            .iter()
            .map(|item| {
                self.progress_items
                    .get(item_id(item))
                    .cloned()
                    .unwrap_or_else(|| item.clone())
            })
            .collect::<Vec<_>>();
        let mut ids = result
            .iter()
            .map(item_id)
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();
        for id in &self.progress_order {
            if ids.contains(id) {
                continue;
            }
            if let Some(item) = self.progress_items.get(id) {
                result.push(item.clone());
                ids.insert(id.clone());
            }
        }
        for item in &self.snapshot.queued_steer {
            if ids.insert(item.id.clone()) {
                result.push(TranscriptItem::User(item.clone()));
            }
        }
        result
    }

    fn set_item(&mut self, item: TranscriptItem) {
        let id = item_id(&item).to_string();
        if !self.progress_items.contains_key(&id) {
            self.progress_order.push(id.clone());
        }
        self.progress_items.insert(id, item);
    }

    fn apply_progress(&mut self, progress: &TranscriptProgress) {
        match progress {
            TranscriptProgress::ItemStarted { item } => self.set_item(item.clone()),
            TranscriptProgress::ItemUpdated { item } => {
                self.set_item(update_to_item(item.clone()));
            }
            TranscriptProgress::ItemFinished { item } => {
                let item = finished_to_item(item.clone());
                let prefix = format!("{}:", item_id(&item));
                self.tool_call_buffers
                    .retain(|key, _| !key.starts_with(&prefix));
                self.set_item(item);
            }
            TranscriptProgress::AssistantDelta {
                message_id,
                content_index,
                kind,
                delta,
            } => self.apply_delta(message_id, *content_index, kind, delta),
        }
    }

    fn apply_delta(
        &mut self,
        message_id: &str,
        content_index: i64,
        kind: &TranscriptDeltaKind,
        delta: &str,
    ) {
        if content_index < 0 {
            return;
        }
        let index = content_index as usize;
        let Some(item) = self.progress_items.get(message_id).cloned().or_else(|| {
            self.snapshot
                .transcript
                .iter()
                .find(|item| item_id(item) == message_id)
                .cloned()
        }) else {
            return;
        };
        let TranscriptItem::Assistant(mut assistant) = item else {
            return;
        };
        let Some(content) = assistant.content.get_mut(index) else {
            return;
        };
        match (kind, content) {
            (TranscriptDeltaKind::Text, AssistantContent::Text(TextContent::Text { text })) => {
                text.push_str(delta);
            }
            (
                TranscriptDeltaKind::Thinking,
                AssistantContent::Thinking(ThinkingContent::Thinking { thinking, .. }),
            ) => thinking.push_str(delta),
            (
                TranscriptDeltaKind::ToolCall,
                AssistantContent::ToolCall(ToolCallContent::ToolCall { input, .. }),
            ) => {
                let key = format!("{message_id}:{index}");
                let existing = self
                    .tool_call_buffers
                    .get(&key)
                    .cloned()
                    .or_else(|| input.as_str().map(str::to_string))
                    .unwrap_or_else(|| input.to_string());
                let buffer = format!("{existing}{delta}");
                *input = serde_json::from_str(&buffer)
                    .unwrap_or_else(|_| JsonValue::String(buffer.clone()));
                self.tool_call_buffers.insert(key, buffer);
            }
            _ => return,
        }
        self.set_item(TranscriptItem::Assistant(assistant));
    }
}

fn item_id(item: &TranscriptItem) -> &str {
    match item {
        TranscriptItem::User(item) => &item.id,
        TranscriptItem::Assistant(item) => &item.id,
        TranscriptItem::Tool(item) => &item.id,
    }
}

fn update_to_item(update: TranscriptItemUpdate) -> TranscriptItem {
    match update {
        TranscriptItemUpdate::Assistant(item) => TranscriptItem::Assistant(item),
        TranscriptItemUpdate::Tool(item) => TranscriptItem::Tool(item),
    }
}

fn finished_to_item(finished: TranscriptItemFinished) -> TranscriptItem {
    match finished {
        TranscriptItemFinished::CompleteAssistant(item)
        | TranscriptItemFinished::ErrorAssistant(item)
        | TranscriptItemFinished::AbortedAssistant(item) => TranscriptItem::Assistant(item),
        TranscriptItemFinished::CompleteTool(item) | TranscriptItemFinished::ErrorTool(item) => {
            TranscriptItem::Tool(item)
        }
    }
}

struct Inner {
    client: PiClient,
    lifecycle: RemoteSessionLifecycle,
    handle: Option<SessionHandle>,
    transcript: Option<TranscriptState>,
    unsubscribe_snapshot: Option<RemoteSessionUnsubscribe>,
    unsubscribe_events: Option<RemoteSessionUnsubscribe>,
    listeners: Vec<(usize, RemoteSessionListener)>,
    next_listener_id: usize,
    on_listener_error: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

/// A stateful, lease-backed remote session facade.
#[derive(Clone)]
pub struct RemoteSession {
    inner: Arc<Mutex<Inner>>,
    operation: Arc<tokio::sync::Mutex<()>>,
}

impl RemoteSession {
    pub fn new(client: PiClient, options: RemoteSessionOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                client,
                lifecycle: RemoteSessionLifecycle::Unbound,
                handle: None,
                transcript: None,
                unsubscribe_snapshot: None,
                unsubscribe_events: None,
                listeners: Vec::new(),
                next_listener_id: 0,
                on_listener_error: options.on_listener_error,
            })),
            operation: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn open(
        client: PiClient,
        session_id: impl Into<String>,
        options: RemoteSessionOptions,
    ) -> Result<Self, RemoteSessionError> {
        let session = Self::new(client, options);
        if let Err(error) = session.open_session(session_id).await {
            let _ = session.dispose().await;
            return Err(error);
        }
        Ok(session)
    }

    pub async fn create(
        client: PiClient,
        options: CreateRemoteSessionOptions,
        session_options: RemoteSessionOptions,
    ) -> Result<Self, RemoteSessionError> {
        let session = Self::new(client, session_options);
        if let Err(error) = session.create_session(options).await {
            let _ = session.dispose().await;
            return Err(error);
        }
        Ok(session)
    }

    pub fn id(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle
            .as_ref()
            .map(|handle| handle.id().to_string())
    }

    pub fn state(&self) -> RemoteSessionState {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        RemoteSessionState {
            lifecycle: inner.lifecycle.clone(),
            snapshot: inner
                .transcript
                .as_ref()
                .map(|state| state.snapshot.clone()),
            transcript: inner
                .transcript
                .as_ref()
                .map(TranscriptState::selected)
                .unwrap_or_default(),
        }
    }

    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .transcript
            .as_ref()
            .map(|state| state.snapshot.clone())
    }

    pub fn phase(&self) -> Option<SessionPhase> {
        self.snapshot().map(|snapshot| snapshot.phase)
    }

    pub fn operation(&self) -> Option<RemoteSessionOperation> {
        match self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lifecycle
        {
            RemoteSessionLifecycle::Busy(operation) => Some(operation),
            _ => None,
        }
    }

    pub fn models(&self) -> Vec<pi_protocol::ModelMetadata> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .client
            .snapshot()
            .map(|snapshot| snapshot.models)
            .unwrap_or_default()
    }

    pub fn sessions(&self) -> Vec<SessionMetadata> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .client
            .snapshot()
            .map(|snapshot| snapshot.sessions)
            .unwrap_or_default()
    }

    pub fn subscribe(
        &self,
        listener: RemoteSessionListener,
    ) -> Result<RemoteSessionUnsubscribe, RemoteSessionError> {
        self.assert_not_disposed()?;
        let id = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let id = inner.next_listener_id;
            inner.next_listener_id += 1;
            inner.listeners.push((id, listener.clone()));
            id
        };
        self.call_listener(listener, self.state());
        let weak = Arc::downgrade(&self.inner);
        Ok(Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .listeners
                    .retain(|(listener_id, _)| *listener_id != id);
            }
        }))
    }

    pub async fn open_session(
        &self,
        session_id: impl Into<String>,
    ) -> Result<(), RemoteSessionError> {
        let client = self.client();
        let session_id = session_id.into();
        self.replace(RemoteSessionOperation::Open, move || async move {
            client
                .acquire_session(
                    &session_id,
                    AcquireSessionOptions {
                        mode: SessionLeaseMode::Exclusive,
                    },
                )
                .await
                .map_err(RemoteSessionError::from)
        })
        .await
    }

    pub async fn create_session(
        &self,
        options: CreateRemoteSessionOptions,
    ) -> Result<(), RemoteSessionError> {
        let client = self.client();
        self.replace(RemoteSessionOperation::Create, move || async move {
            client
                .start_session(
                    Some(options.cwd),
                    None,
                    options.model,
                    options.thinking_level,
                    AcquireSessionOptions {
                        mode: SessionLeaseMode::Exclusive,
                    },
                )
                .await
                .map_err(RemoteSessionError::from)
        })
        .await
    }

    pub async fn submit(&self, text: impl Into<String>) -> Result<(), RemoteSessionError> {
        let text = text.into().trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        let phase = self.phase().ok_or(RemoteSessionError::Unbound)?;
        if !matches!(phase, SessionPhase::Idle | SessionPhase::Turn) {
            return Err(RemoteSessionError::InvalidPhase(format!("{phase:?}")));
        }
        let handle = self.handle()?;
        self.run_operation(RemoteSessionOperation::Submit, async move {
            if matches!(phase, SessionPhase::Idle) {
                handle.prompt(&text).await.map(|_| ()).map_err(Into::into)
            } else {
                handle.steer(&text).await.map(|_| ()).map_err(Into::into)
            }
        })
        .await
    }

    pub async fn abort(&self) -> Result<(), RemoteSessionError> {
        let handle = self.handle()?;
        self.run_operation(RemoteSessionOperation::Abort, async move {
            handle.abort().await.map(|_| ()).map_err(Into::into)
        })
        .await
    }

    pub async fn set_model(&self, model: ModelRef) -> Result<(), RemoteSessionError> {
        self.require_idle("change model")?;
        let handle = self.handle()?;
        self.run_operation(RemoteSessionOperation::SetModel, async move {
            handle
                .set_model(model)
                .await
                .map(|_| ())
                .map_err(Into::into)
        })
        .await
    }

    pub async fn set_thinking(
        &self,
        level: pi_protocol::ThinkingLevel,
    ) -> Result<(), RemoteSessionError> {
        self.require_idle("change thinking level")?;
        let handle = self.handle()?;
        self.run_operation(RemoteSessionOperation::SetThinking, async move {
            handle
                .set_thinking(level)
                .await
                .map(|_| ())
                .map_err(Into::into)
        })
        .await
    }

    pub async fn reconnect(&self) -> Result<(), RemoteSessionError> {
        let session_id = self.id().ok_or(RemoteSessionError::Unbound)?;
        let client = self.client();
        self.run_operation(RemoteSessionOperation::Reconnect, async move {
            client.reconnect().await.map_err(RemoteSessionError::from)?;
            let handle = client
                .acquire_session(
                    &session_id,
                    AcquireSessionOptions {
                        mode: SessionLeaseMode::Exclusive,
                    },
                )
                .await
                .map_err(RemoteSessionError::from)?;
            self.bind(handle)?;
            Ok(())
        })
        .await
    }

    pub async fn dispose(&self) -> Result<(), RemoteSessionError> {
        let (handle, snapshot_unsubscribe, event_unsubscribe) = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            if matches!(inner.lifecycle, RemoteSessionLifecycle::Disposed) {
                return Ok(());
            }
            inner.lifecycle = RemoteSessionLifecycle::Disposed;
            (
                inner.handle.take(),
                inner.unsubscribe_snapshot.take(),
                inner.unsubscribe_events.take(),
            )
        };
        snapshot_unsubscribe
            .iter()
            .for_each(|unsubscribe| unsubscribe());
        event_unsubscribe
            .iter()
            .for_each(|unsubscribe| unsubscribe());
        if let Some(handle) = handle {
            handle.dispose().await.map_err(RemoteSessionError::from)?;
        }
        self.notify();
        Ok(())
    }

    fn client(&self) -> PiClient {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .client
            .clone()
    }

    fn handle(&self) -> Result<SessionHandle, RemoteSessionError> {
        self.assert_available()?;
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle
            .clone()
            .ok_or(RemoteSessionError::Unbound)
    }

    fn require_idle(&self, description: &str) -> Result<(), RemoteSessionError> {
        self.assert_available()?;
        match self.phase() {
            Some(SessionPhase::Idle) => Ok(()),
            Some(phase) => Err(RemoteSessionError::InvalidOperation {
                operation: description.to_string(),
                phase: format!("{phase:?}"),
            }),
            None => Err(RemoteSessionError::Unbound),
        }
    }

    fn assert_not_disposed(&self) -> Result<(), RemoteSessionError> {
        if matches!(
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .lifecycle,
            RemoteSessionLifecycle::Disposed
        ) {
            Err(RemoteSessionError::Disposed)
        } else {
            Ok(())
        }
    }

    fn assert_available(&self) -> Result<(), RemoteSessionError> {
        self.assert_not_disposed()?;
        if let RemoteSessionLifecycle::Busy(operation) = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lifecycle
        {
            return Err(RemoteSessionError::Busy(operation));
        }
        Ok(())
    }

    async fn replace<F, Fut>(
        &self,
        operation: RemoteSessionOperation,
        prepare: F,
    ) -> Result<(), RemoteSessionError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<SessionHandle, RemoteSessionError>>,
    {
        self.assert_available()?;
        if self.handle().is_ok() && !matches!(self.phase(), Some(SessionPhase::Idle)) {
            return Err(RemoteSessionError::InvalidOperation {
                operation: format!("{operation:?}"),
                phase: self
                    .phase()
                    .map(|phase| format!("{phase:?}"))
                    .unwrap_or_else(|| "unknown".into()),
            });
        }
        let previous = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle
            .clone();
        self.run_operation(operation, async move {
            let next = prepare().await?;
            let snapshot = next.snapshot().ok_or_else(|| {
                RemoteSessionError::Client(format!(
                    "Session {} did not provide a snapshot",
                    next.id()
                ))
            })?;
            if let Some(previous) = previous {
                if previous.id() != next.id() && previous.attached() {
                    previous.detach().await.map_err(RemoteSessionError::from)?;
                }
            }
            self.bind_with_snapshot(next, snapshot)
        })
        .await
    }

    async fn run_operation<F>(
        &self,
        operation: RemoteSessionOperation,
        run: F,
    ) -> Result<(), RemoteSessionError>
    where
        F: std::future::Future<Output = Result<(), RemoteSessionError>>,
    {
        self.assert_available()?;
        let _guard = self.operation.lock().await;
        self.assert_available()?;
        {
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .lifecycle = RemoteSessionLifecycle::Busy(operation);
        }
        self.notify();
        let result = run.await;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            if !matches!(inner.lifecycle, RemoteSessionLifecycle::Disposed) {
                inner.lifecycle = if inner.handle.is_some() {
                    RemoteSessionLifecycle::Ready
                } else {
                    RemoteSessionLifecycle::Unbound
                };
            }
        }
        self.notify();
        result
    }

    fn bind(&self, handle: SessionHandle) -> Result<(), RemoteSessionError> {
        let snapshot = handle.snapshot().ok_or_else(|| {
            RemoteSessionError::Client(format!(
                "Session {} did not provide a snapshot",
                handle.id()
            ))
        })?;
        self.bind_with_snapshot(handle, snapshot)
    }

    fn bind_with_snapshot(
        &self,
        handle: SessionHandle,
        snapshot: SessionSnapshot,
    ) -> Result<(), RemoteSessionError> {
        let (old_snapshot, old_events) = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            (
                inner.unsubscribe_snapshot.take(),
                inner.unsubscribe_events.take(),
            )
        };
        old_snapshot.iter().for_each(|unsubscribe| unsubscribe());
        old_events.iter().for_each(|unsubscribe| unsubscribe());

        let weak = Arc::downgrade(&self.inner);
        let weak_for_snapshot = weak.clone();
        let unsubscribe_snapshot = handle.subscribe(move |snapshot| {
            if let Some(inner) = weak_for_snapshot.upgrade() {
                if let Ok(mut inner) = inner.lock() {
                    if let Some(transcript) = inner.transcript.as_mut() {
                        if snapshot.id == transcript.snapshot.id
                            && snapshot.revision < transcript.snapshot.revision
                        {
                            return;
                        }
                        transcript.snapshot = snapshot.clone();
                    }
                }
                notify_arc(&inner);
            }
        });
        let unsubscribe_events = handle.on_event(move |event| {
            if let Some(inner) = weak.upgrade() {
                if let Ok(mut inner_guard) = inner.lock() {
                    match event {
                        ServerEvent::SessionProgress { progress, .. } => {
                            if let Some(transcript) = inner_guard.transcript.as_mut() {
                                transcript.apply_progress(progress);
                            }
                        }
                        ServerEvent::SessionRemoved { .. } => {
                            inner_guard.handle = None;
                            inner_guard.transcript = None;
                            if !matches!(inner_guard.lifecycle, RemoteSessionLifecycle::Busy(_)) {
                                inner_guard.lifecycle = RemoteSessionLifecycle::Unbound;
                            }
                        }
                        ServerEvent::SessionSnapshot { snapshot } => {
                            if let Some(transcript) = inner_guard.transcript.as_mut() {
                                if snapshot.id == transcript.snapshot.id
                                    && snapshot.revision >= transcript.snapshot.revision
                                {
                                    transcript.snapshot = snapshot.clone();
                                }
                            }
                        }
                        ServerEvent::ServerSnapshot { .. } => {}
                    }
                }
                notify_arc(&inner);
            }
        });
        {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner.handle = Some(handle);
            inner.transcript = Some(TranscriptState::new(snapshot));
            inner.unsubscribe_snapshot = Some(unsubscribe_snapshot);
            inner.unsubscribe_events = Some(unsubscribe_events);
            inner.lifecycle = RemoteSessionLifecycle::Ready;
        }
        self.notify();
        Ok(())
    }

    fn notify(&self) {
        notify_arc(&self.inner);
    }

    fn call_listener(&self, listener: RemoteSessionListener, state: RemoteSessionState) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(state)));
        if result.is_err() {
            let callback = self
                .inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .on_listener_error
                .clone();
            if let Some(callback) = callback {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    callback("remote session listener panicked".to_string())
                }));
            }
        }
    }
}

fn notify_arc(inner: &Arc<Mutex<Inner>>) {
    let (state, listeners, on_listener_error) = {
        let inner_guard = inner.lock().unwrap_or_else(|error| error.into_inner());
        let state = RemoteSessionState {
            lifecycle: inner_guard.lifecycle.clone(),
            snapshot: inner_guard
                .transcript
                .as_ref()
                .map(|transcript| transcript.snapshot.clone()),
            transcript: inner_guard
                .transcript
                .as_ref()
                .map(TranscriptState::selected)
                .unwrap_or_default(),
        };
        (
            state,
            inner_guard
                .listeners
                .iter()
                .map(|(_, listener)| listener.clone())
                .collect::<Vec<_>>(),
            inner_guard.on_listener_error.clone(),
        )
    };
    for listener in listeners {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(state.clone())))
            .is_err()
        {
            if let Some(callback) = &on_listener_error {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    callback("remote session listener panicked".to_string())
                }));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            id: "s1".into(),
            name: None,
            cwd: "/tmp".into(),
            created_at: 1,
            updated_at: 1,
            phase: SessionPhase::Idle,
            model: ModelRef {
                provider: "faux".into(),
                id: "faux-1".into(),
            },
            thinking_level: pi_protocol::ThinkingLevel::Medium,
            attached: true,
            locked: false,
            revision: 1,
            transcript: Vec::new(),
            queued_steer: Vec::new(),
            queued_steer_count: 0,
        }
    }

    fn assistant_item() -> TranscriptItem {
        TranscriptItem::Assistant(AssistantTranscriptItem {
            id: "a1".into(),
            role: "assistant".into(),
            content: vec![AssistantContent::Text(TextContent::Text {
                text: String::new(),
            })],
            model: ModelRef {
                provider: "faux".into(),
                id: "faux-1".into(),
            },
            response_model: None,
            usage: None,
            timestamp: 1,
            status: AssistantStatus::Streaming,
            stop_reason: None,
            error_message: None,
        })
    }

    #[test]
    fn reducer_appends_incremental_text_and_preserves_order() {
        let mut state = TranscriptState::new(snapshot());
        state.apply_progress(&TranscriptProgress::ItemStarted {
            item: assistant_item(),
        });
        state.apply_progress(&TranscriptProgress::AssistantDelta {
            message_id: "a1".into(),
            content_index: 0,
            kind: TranscriptDeltaKind::Text,
            delta: "hello".into(),
        });
        state.apply_progress(&TranscriptProgress::AssistantDelta {
            message_id: "a1".into(),
            content_index: 0,
            kind: TranscriptDeltaKind::Text,
            delta: " world".into(),
        });
        assert_eq!(state.selected().len(), 1);
        let TranscriptItem::Assistant(item) = &state.selected()[0] else {
            panic!("expected assistant")
        };
        assert_eq!(
            item.content,
            vec![AssistantContent::Text(TextContent::Text {
                text: "hello world".into()
            })]
        );
    }

    #[test]
    fn reducer_keeps_partial_tool_json_until_it_becomes_valid() {
        let mut state = TranscriptState::new(snapshot());
        state.apply_progress(&TranscriptProgress::ItemStarted {
            item: TranscriptItem::Assistant(AssistantTranscriptItem {
                content: vec![AssistantContent::ToolCall(ToolCallContent::ToolCall {
                    tool_call_id: "c1".into(),
                    tool_name: "read".into(),
                    input: JsonValue::String("{".into()),
                })],
                ..match assistant_item() {
                    TranscriptItem::Assistant(item) => item,
                    _ => unreachable!(),
                }
            }),
        });
        state.apply_progress(&TranscriptProgress::AssistantDelta {
            message_id: "a1".into(),
            content_index: 0,
            kind: TranscriptDeltaKind::ToolCall,
            delta: "\"path\":\"x\"}".into(),
        });
        let TranscriptItem::Assistant(item) = &state.selected()[0] else {
            panic!("expected assistant")
        };
        let AssistantContent::ToolCall(ToolCallContent::ToolCall { input, .. }) = &item.content[0]
        else {
            panic!("expected tool call")
        };
        assert_eq!(input, &serde_json::json!({"path": "x"}));
    }

    #[test]
    fn reducer_ignores_unknown_or_negative_deltas() {
        let mut state = TranscriptState::new(snapshot());
        state.apply_progress(&TranscriptProgress::AssistantDelta {
            message_id: "missing".into(),
            content_index: -1,
            kind: TranscriptDeltaKind::Text,
            delta: "ignored".into(),
        });
        assert!(state.selected().is_empty());
    }
}
