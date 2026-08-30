//! Service boundary and in-memory test service — port of
//! `packages/server/src/types.ts` + `packages/server/src/testing/service.ts`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_protocol::{
    CommandResult, ModelMetadata, ModelRef, SessionMetadata, SessionPhase, SessionSnapshot,
    ThinkingLevel, TranscriptItem, TranscriptProgress, UserContent, UserTranscriptItem,
};

use crate::errors::PiServerError;
use crate::types::{PromptInput, SteerInput};

/// A one-shot deferred value used by deterministic server fixtures and by
/// runtimes that need to keep an operation pending until a test releases it.
/// Resolving more than once is intentionally a no-op, matching a settled
/// JavaScript promise.
#[derive(Clone)]
pub struct Deferred<T> {
    state: Arc<DeferredState<T>>,
}

struct DeferredState<T> {
    value: tokio::sync::watch::Sender<Option<T>>,
}

impl<T> Deferred<T> {
    pub fn new() -> Self {
        let (value, _) = tokio::sync::watch::channel(None);
        Self {
            state: Arc::new(DeferredState { value }),
        }
    }

    pub fn resolve(&self, value: T) {
        let mut value = Some(value);
        self.state.value.send_if_modified(|slot| {
            if slot.is_none() {
                *slot = value.take();
                true
            } else {
                false
            }
        });
    }

    pub fn is_resolved(&self) -> bool {
        self.state.value.borrow().is_some()
    }

    pub async fn wait(&self) -> T
    where
        T: Clone,
    {
        let mut receiver = self.state.value.subscribe();
        loop {
            if let Some(value) = receiver.borrow().as_ref().cloned() {
                return value;
            }
            // The sender lives in the same state struct; the loop cannot
            // outlive it, so a dropped sender is a caller-contract defect.
            #[allow(clippy::panic)]
            if receiver.changed().await.is_err() {
                panic!("deferred sender remains alive while state is alive");
            }
        }
    }

    pub fn promise(&self) -> impl Future<Output = T> + '_
    where
        T: Clone,
    {
        self.wait()
    }
}

impl<T> Default for Deferred<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A pending runtime operation. The server awaits this after releasing the
/// runtime mutex, which lets steer/abort commands reach a pending prompt.
pub struct RuntimeWait {
    future: Pin<Box<dyn Future<Output = Result<(), PiServerError>> + Send>>,
}

impl RuntimeWait {
    pub fn new(future: impl Future<Output = Result<(), PiServerError>> + Send + 'static) -> Self {
        Self {
            future: Box::pin(future),
        }
    }

    pub async fn wait(self) -> Result<(), PiServerError> {
        self.future.await
    }
}

/// One acquired durable session. Conflicting operations reject rather than queue.
pub trait PiSessionRuntime: Send {
    fn snapshot(&mut self) -> Result<SessionSnapshot, PiServerError>;
    fn get_phase(&self) -> SessionPhase;
    fn prompt(&mut self, input: crate::types::PromptInput) -> Result<(), PiServerError>;
    fn steer(&mut self, input: crate::types::SteerInput) -> Result<(), PiServerError>;
    fn abort(&mut self) -> Result<(), PiServerError>;
    fn set_model(&mut self, model: ModelRef) -> Result<(), PiServerError>;
    fn set_thinking(&mut self, thinking_level: ThinkingLevel) -> Result<(), PiServerError>;
    fn subscribe(&mut self, listener: EventListener) -> Result<Unsubscribe, PiServerError>;
    fn dispose(&mut self) -> Result<(), PiServerError>;

    /// Return a pending operation created by the preceding mutating call.
    /// Implementations that complete synchronously keep the default.
    fn take_pending_operation(&mut self) -> Option<RuntimeWait> {
        None
    }
}

pub type EventListener = Arc<dyn Fn(PiSessionRuntimeEvent) + Send + Sync>;
pub type Unsubscribe = Box<dyn Fn() + Send + Sync>;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum PiSessionRuntimeEvent {
    Snapshot,
    Progress(TranscriptProgress),
    Error(PiServerError),
}

pub trait PiServerService: Send + Sync {
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiServerError>;
    fn list_models(&self) -> Result<Vec<ModelMetadata>, PiServerError>;
    fn create_session(
        &mut self,
        options: crate::types::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError>;
    fn open_session(
        &mut self,
        session_id: String,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError>;

    /// Whether a create result reports the newly attached connection. The
    /// deterministic test service follows the upstream contract; the legacy
    /// in-memory service keeps its historical result shape so older clients
    /// reconcile by issuing an explicit attach.
    fn create_result_attached(&self) -> bool {
        true
    }
}

impl PiServerService for Box<dyn PiServerService> {
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiServerError> {
        (**self).list_sessions()
    }
    fn list_models(&self) -> Result<Vec<ModelMetadata>, PiServerError> {
        (**self).list_models()
    }
    fn create_session(
        &mut self,
        options: crate::types::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        (**self).create_session(options)
    }
    fn open_session(
        &mut self,
        session_id: String,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        (**self).open_session(session_id)
    }

    fn create_result_attached(&self) -> bool {
        (**self).create_result_attached()
    }
}

// ---------------------------------------------------------------------------
// In-memory test service
// ---------------------------------------------------------------------------

/// An in-memory PiServerService: sessions stored as `TestSession` runtimes.
#[derive(Clone)]
pub struct InMemoryService {
    inner: Arc<Mutex<InMemoryServiceInner>>,
}

struct InMemoryServiceInner {
    _next_id: u64,
    sessions: BTreeMap<String, SessionSnapshot>,
    listeners: BTreeMap<String, Vec<Option<EventListener>>>,
    models: Vec<ModelMetadata>,
}

impl InMemoryService {
    pub fn new(models: Vec<ModelMetadata>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InMemoryServiceInner {
                _next_id: 1,
                sessions: BTreeMap::new(),
                listeners: BTreeMap::new(),
                models,
            })),
        }
    }

    fn snapshot_for(&self, session_id: &str) -> Result<SessionSnapshot, PiServerError> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.sessions.get(session_id).cloned().ok_or_else(|| {
            PiServerError::new(
                pi_protocol::ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            )
        })
    }

    /// Fire a runtime event at all live listeners for `session_id` (used to
    /// script progress/error fan-out and terminal-close in tests). Best-effort
    /// like the upstream test service: the listener registry is append-only.
    pub fn emit(&self, session_id: &str, event: PiSessionRuntimeEvent) {
        let listeners = {
            let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner.listeners.get(session_id).cloned().unwrap_or_default()
        };
        for listener in listeners.into_iter().flatten() {
            listener(event.clone());
        }
    }

    /// Settle a running session back to Idle and emit a `Snapshot` (used by
    /// tests to simulate a streaming turn finishing so the manager's
    /// dispose-on-idle logic can be exercised).
    pub fn settle_idle(&self, session_id: &str) -> Result<(), PiServerError> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let snap = inner.sessions.get_mut(session_id).ok_or_else(|| {
            PiServerError::new(
                pi_protocol::ProtocolErrorCode::NotFound,
                "Session not found".to_string(),
            )
        })?;
        snap.phase = SessionPhase::Idle;
        Ok(())
    }
}

impl PiServerService for InMemoryService {
    fn create_result_attached(&self) -> bool {
        false
    }

    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiServerError> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        Ok(inner.sessions.values().map(metadata_of).collect())
    }
    fn list_models(&self) -> Result<Vec<ModelMetadata>, PiServerError> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        Ok(inner.models.clone())
    }
    fn create_session(
        &mut self,
        options: crate::types::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.sessions.contains_key(&options.id) {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::InvalidRequest,
                format!("Session already exists: {}", options.id),
            ));
        }
        let now = pi_ai::types::now_ms() as i64;
        let model = options.model.clone().unwrap_or(ModelRef {
            provider: "faux".into(),
            id: "faux-1".into(),
        });
        let snapshot = SessionSnapshot {
            id: options.id.clone(),
            name: options.name.clone(),
            cwd: options.cwd.clone().unwrap_or_else(|| ".".to_string()),
            created_at: now,
            updated_at: now,
            phase: SessionPhase::Idle,
            model,
            thinking_level: options.thinking_level.unwrap_or(ThinkingLevel::Off),
            attached: false,
            locked: false,
            revision: 0,
            transcript: Vec::new(),
            queued_steer: Vec::new(),
            queued_steer_count: 0,
        };
        inner.sessions.insert(options.id.clone(), snapshot);
        let runtime = TestSession::new(self.clone(), options.id);
        Ok(runtime)
    }
    fn open_session(
        &mut self,
        session_id: String,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        {
            let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            if !inner.sessions.contains_key(&session_id) {
                return Err(PiServerError::new(
                    pi_protocol::ProtocolErrorCode::NotFound,
                    format!("Session not found: {session_id}"),
                ));
            }
        }
        let runtime = TestSession::new(self.clone(), session_id);
        Ok(runtime)
    }
}

fn metadata_of(snapshot: &SessionSnapshot) -> SessionMetadata {
    SessionMetadata {
        id: snapshot.id.clone(),
        created_at: snapshot.created_at,
        updated_at: Some(snapshot.updated_at),
        parent_session_id: None,
        session_name: snapshot.name.clone(),
        cwd: Some(snapshot.cwd.clone()),
    }
}

/// A minimal in-memory session runtime bound to a service mutation.
struct TestSession {
    service: InMemoryService,
    session_id: String,
}

impl TestSession {
    #[allow(clippy::new_ret_no_self)]
    fn new(service: InMemoryService, session_id: String) -> Arc<Mutex<dyn PiSessionRuntime>> {
        Arc::new(Mutex::new(Self {
            service,
            session_id,
        }))
    }
}

impl PiSessionRuntime for TestSession {
    fn snapshot(&mut self) -> Result<SessionSnapshot, PiServerError> {
        self.service.snapshot_for(&self.session_id)
    }
    fn get_phase(&self) -> SessionPhase {
        let inner = self
            .service
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        inner
            .sessions
            .get(&self.session_id)
            .map(|s| s.phase.clone())
            .unwrap_or(SessionPhase::Idle)
    }
    fn prompt(&mut self, input: crate::types::PromptInput) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "A prompt is already running",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let item = UserTranscriptItem {
            id,
            role: "user".to_string(),
            content: vec![UserContent::Text(pi_protocol::TextContent::Text {
                text: input.text.clone(),
            })],
            timestamp: pi_ai::types::now_ms() as i64,
        };
        self.update(|snap| {
            snap.transcript.push(TranscriptItem::User(item.clone()));
            snap.phase = SessionPhase::Turn;
        })?;
        Ok(())
    }
    fn steer(&mut self, input: crate::types::SteerInput) -> Result<(), PiServerError> {
        if self.get_phase() == SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "There is no active prompt to steer",
            ));
        }
        let item = UserTranscriptItem {
            id: uuid::Uuid::new_v4().to_string(),
            role: "user".to_string(),
            content: vec![UserContent::Text(pi_protocol::TextContent::Text {
                text: input.text.clone(),
            })],
            timestamp: pi_ai::types::now_ms() as i64,
        };
        self.update(|snap| {
            snap.queued_steer.push(item.clone());
            snap.queued_steer_count += 1;
            snap.phase = SessionPhase::Turn;
        })?;
        Ok(())
    }
    fn abort(&mut self) -> Result<(), PiServerError> {
        if self.get_phase() == SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "There is no active prompt to abort",
            ));
        }
        self.update(|snap| {
            snap.phase = SessionPhase::Idle;
            snap.queued_steer.clear();
            snap.queued_steer_count = 0;
        })?;
        Ok(())
    }
    fn set_model(&mut self, model: ModelRef) -> Result<(), PiServerError> {
        self.update(|snap| snap.model = model)?;
        Ok(())
    }
    fn set_thinking(&mut self, thinking_level: ThinkingLevel) -> Result<(), PiServerError> {
        self.update(|snap| snap.thinking_level = thinking_level)?;
        Ok(())
    }
    fn subscribe(&mut self, listener: EventListener) -> Result<Unsubscribe, PiServerError> {
        let mut inner = self
            .service
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let listeners = inner.listeners.entry(self.session_id.clone()).or_default();
        listeners.push(Some(listener));
        let index = listeners.len() - 1;
        let listeners = self.service.inner.clone();
        let session_id = self.session_id.clone();
        Ok(Box::new(move || {
            if let Some(slots) = listeners
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .listeners
                .get_mut(&session_id)
            {
                if let Some(slot) = slots.get_mut(index) {
                    *slot = None;
                }
            }
        }))
    }
    fn dispose(&mut self) -> Result<(), PiServerError> {
        // Durable sessions remain in storage after a runtime is released;
        // only the live runtime is disposed. This is what permits a later
        // attach or a server restart to restore the persisted session.
        Ok(())
    }
}

impl TestSession {
    fn update(&self, f: impl FnOnce(&mut SessionSnapshot)) -> Result<(), PiServerError> {
        let mut inner = self
            .service
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let snap = inner.sessions.get_mut(&self.session_id).ok_or_else(|| {
            PiServerError::new(
                pi_protocol::ProtocolErrorCode::NotFound,
                "Session not found".to_string(),
            )
        })?;
        f(snap);
        snap.updated_at = pi_ai::types::now_ms() as i64;
        snap.revision += 1;
        let listeners = inner
            .listeners
            .get(&self.session_id)
            .cloned()
            .unwrap_or_default();
        drop(inner);
        for listener in listeners.into_iter().flatten() {
            listener(PiSessionRuntimeEvent::Snapshot);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Deterministic upstream-style testing service
// ---------------------------------------------------------------------------

/// The model used by the upstream `testing/service.ts` harness.
pub fn test_model() -> ModelMetadata {
    ModelMetadata {
        provider: "test".to_string(),
        id: "small".to_string(),
        name: "Test Small".to_string(),
        api: "test-api".to_string(),
        reasoning: true,
        input: vec![
            pi_protocol::ModelInput::Text,
            pi_protocol::ModelInput::Image,
        ],
        context_window: 16_000,
        max_tokens: 2_000,
        cost: pi_protocol::ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        supported_thinking_levels: vec![
            ThinkingLevel::Off,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
        authenticated: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestPromptOutcome {
    Complete,
    Aborted,
}

struct PendingTestPrompt {
    input: PromptInput,
    done: Deferred<TestPromptOutcome>,
    wait_taken: bool,
}

struct TestSessionRuntimeState {
    listeners: Vec<Option<EventListener>>,
    pending_prompt: Option<PendingTestPrompt>,
    steers: Vec<PromptInput>,
    dispose_count: Arc<AtomicUsize>,
    disposed: Deferred<()>,
}

/// A deterministic runtime with the same deferred prompt controls as the
/// upstream `TestSessionRuntime`. It is public so integration tests can drive
/// progress, terminal errors, prompt completion, and disposal without a live
/// provider or network.
#[derive(Clone)]
pub struct TestSessionRuntime {
    stored: Arc<Mutex<SessionSnapshot>>,
    state: Arc<Mutex<TestSessionRuntimeState>>,
    on_dispose: Arc<dyn Fn() + Send + Sync>,
}

impl TestSessionRuntime {
    fn new(stored: Arc<Mutex<SessionSnapshot>>, on_dispose: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            stored,
            state: Arc::new(Mutex::new(TestSessionRuntimeState {
                listeners: Vec::new(),
                pending_prompt: None,
                steers: Vec::new(),
                dispose_count: Arc::new(AtomicUsize::new(0)),
                disposed: Deferred::new(),
            })),
            on_dispose,
        }
    }

    pub fn disposed(&self) -> Deferred<()> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .disposed
            .clone()
    }

    pub fn dispose_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dispose_count
            .load(Ordering::SeqCst)
    }

    pub fn steers(&self) -> Vec<PromptInput> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .steers
            .clone()
    }

    pub fn set_phase(&self, phase: SessionPhase) {
        self.stored
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase = phase;
    }

    pub fn finish_prompt(&self) -> Result<(), PiServerError> {
        self.resolve_prompt(TestPromptOutcome::Complete)
    }

    pub fn emit_progress(&self, progress: TranscriptProgress) {
        self.emit(PiSessionRuntimeEvent::Progress(progress));
    }

    pub fn emit_snapshot(&self) {
        self.emit(PiSessionRuntimeEvent::Snapshot);
    }

    pub fn emit_error(&self, error: PiServerError) {
        self.emit(PiSessionRuntimeEvent::Error(error));
    }

    fn emit(&self, event: PiSessionRuntimeEvent) {
        let listeners = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .listeners
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(event.clone());
        }
    }

    fn update(&self, update: impl FnOnce(&mut SessionSnapshot)) {
        {
            let mut snapshot = self
                .stored
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            update(&mut snapshot);
            snapshot.revision += 1;
            snapshot.updated_at += 1;
        }
        self.emit_snapshot();
    }

    fn resolve_prompt(&self, outcome: TestPromptOutcome) -> Result<(), PiServerError> {
        let done = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_prompt
            .as_ref()
            .map(|pending| pending.done.clone())
            .ok_or_else(|| {
                PiServerError::new(
                    pi_protocol::ProtocolErrorCode::Busy,
                    "There is no active prompt",
                )
            })?;
        done.resolve(outcome);
        Ok(())
    }

    fn complete_prompt(&self, input: PromptInput, outcome: TestPromptOutcome) {
        let snapshot = self
            .stored
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let assistant = pi_protocol::AssistantTranscriptItem {
            id: format!("assistant-{}", snapshot.revision + 1),
            role: "assistant".to_string(),
            content: vec![pi_protocol::AssistantContent::Text(
                pi_protocol::TextContent::Text {
                    text: if outcome == TestPromptOutcome::Complete {
                        format!("reply:{}", input.text)
                    } else {
                        String::new()
                    },
                },
            )],
            status: if outcome == TestPromptOutcome::Complete {
                pi_protocol::AssistantStatus::Complete
            } else {
                pi_protocol::AssistantStatus::Aborted
            },
            model: snapshot.model,
            response_model: None,
            usage: None,
            stop_reason: if outcome == TestPromptOutcome::Complete {
                Some(pi_protocol::AssistantStopReason::Stop)
            } else {
                Some(pi_protocol::AssistantStopReason::Aborted)
            },
            error_message: None,
            timestamp: snapshot.revision + 1,
        };
        self.update(|snapshot| {
            snapshot.phase = SessionPhase::Idle;
            snapshot
                .transcript
                .push(TranscriptItem::Assistant(assistant));
        });
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_prompt = None;
    }
}

impl PiSessionRuntime for TestSessionRuntime {
    fn snapshot(&mut self) -> Result<SessionSnapshot, PiServerError> {
        Ok(self
            .stored
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone())
    }

    fn get_phase(&self) -> SessionPhase {
        self.stored
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            .clone()
    }

    fn prompt(&mut self, input: PromptInput) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "A prompt is already running",
            ));
        }
        let revision = self
            .stored
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revision
            + 1;
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_prompt = Some(PendingTestPrompt {
            input: input.clone(),
            done: Deferred::new(),
            wait_taken: false,
        });
        self.update(|snapshot| {
            snapshot.phase = SessionPhase::Turn;
            snapshot
                .transcript
                .push(TranscriptItem::User(UserTranscriptItem {
                    id: format!("user-{revision}"),
                    role: "user".to_string(),
                    content: vec![UserContent::Text(pi_protocol::TextContent::Text {
                        text: input.text.clone(),
                    })],
                    timestamp: revision,
                }));
        });
        Ok(())
    }

    fn steer(&mut self, input: SteerInput) -> Result<(), PiServerError> {
        if self.get_phase() == SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "There is no active prompt to steer",
            ));
        }
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .steers
            .push(PromptInput {
                text: input.text.clone(),
            });
        let revision = self
            .stored
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .revision
            + 1;
        self.update(|snapshot| {
            snapshot.queued_steer.push(UserTranscriptItem {
                id: format!("steer-{revision}"),
                role: "user".to_string(),
                content: vec![UserContent::Text(pi_protocol::TextContent::Text {
                    text: input.text.clone(),
                })],
                timestamp: revision,
            });
            snapshot.queued_steer_count += 1;
        });
        Ok(())
    }

    fn abort(&mut self) -> Result<(), PiServerError> {
        self.resolve_prompt(TestPromptOutcome::Aborted)
    }

    fn set_model(&mut self, model: ModelRef) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "Session is busy",
            ));
        }
        self.update(|snapshot| snapshot.model = model);
        Ok(())
    }

    fn set_thinking(&mut self, thinking_level: ThinkingLevel) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                pi_protocol::ProtocolErrorCode::Busy,
                "Session is busy",
            ));
        }
        self.update(|snapshot| snapshot.thinking_level = thinking_level);
        Ok(())
    }

    fn subscribe(&mut self, listener: EventListener) -> Result<Unsubscribe, PiServerError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.listeners.push(Some(listener));
        let index = state.listeners.len() - 1;
        let state = self.state.clone();
        Ok(Box::new(move || {
            if let Some(slot) = state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .listeners
                .get_mut(index)
            {
                *slot = None;
            }
        }))
    }

    fn dispose(&mut self) -> Result<(), PiServerError> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dispose_count
            .fetch_add(1, Ordering::SeqCst);
        (self.on_dispose)();
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .disposed
            .resolve(());
        Ok(())
    }

    fn take_pending_operation(&mut self) -> Option<RuntimeWait> {
        let pending = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let pending = state.pending_prompt.as_mut()?;
            if pending.wait_taken {
                return None;
            }
            pending.wait_taken = true;
            (pending.input.clone(), pending.done.clone())
        };
        let runtime = self.clone();
        Some(RuntimeWait::new(async move {
            let outcome = pending.1.wait().await;
            runtime.complete_prompt(pending.0, outcome);
            Ok(())
        }))
    }
}

struct TestServerServiceInner {
    sessions: BTreeMap<String, Arc<Mutex<SessionSnapshot>>>,
    runtimes: BTreeMap<String, Vec<TestSessionRuntime>>,
    locked: BTreeSet<String>,
    models: Vec<ModelMetadata>,
    last_created_id: Option<String>,
}

/// A deterministic service harness matching the upstream testing service.
#[derive(Clone)]
pub struct TestServerService {
    inner: Arc<Mutex<TestServerServiceInner>>,
}

impl TestServerService {
    pub fn new() -> Self {
        Self::with_models(vec![test_model()])
    }

    pub fn with_models(models: Vec<ModelMetadata>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TestServerServiceInner {
                sessions: BTreeMap::new(),
                runtimes: BTreeMap::new(),
                locked: BTreeSet::new(),
                models,
                last_created_id: None,
            })),
        }
    }

    pub fn seed(&self, id: impl Into<String>) {
        let id = id.into();
        let snapshot = SessionSnapshot {
            id: id.clone(),
            name: Some(format!("Session {id}")),
            cwd: "/tmp/pi-server-conformance".to_string(),
            created_at: 1,
            updated_at: 1,
            phase: SessionPhase::Idle,
            model: ModelRef {
                provider: "test".to_string(),
                id: "small".to_string(),
            },
            thinking_level: ThinkingLevel::Off,
            attached: false,
            locked: false,
            revision: 0,
            transcript: Vec::new(),
            queued_steer: Vec::new(),
            queued_steer_count: 0,
        };
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .insert(id, Arc::new(Mutex::new(snapshot)));
    }

    pub fn seed_with(&self, snapshot: SessionSnapshot) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .insert(snapshot.id.clone(), Arc::new(Mutex::new(snapshot)));
    }

    pub fn last_created_id(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_created_id
            .clone()
    }

    pub fn latest_runtime(&self, id: &str) -> Option<TestSessionRuntime> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtimes
            .get(id)
            .and_then(|runtimes| runtimes.last())
            .cloned()
    }

    pub fn runtime_count(&self, id: &str) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtimes
            .get(id)
            .map_or(0, Vec::len)
    }

    pub fn is_locked(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .locked
            .contains(id)
    }

    pub fn lock_session(&self, id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .locked
            .insert(id.to_string());
    }

    fn acquire(&self, id: &str) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        let (stored, inner_ref) = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let stored = inner.sessions.get(id).cloned().ok_or_else(|| {
                PiServerError::new(
                    pi_protocol::ProtocolErrorCode::NotFound,
                    format!("Unknown session: {id}"),
                )
            })?;
            if inner.locked.contains(id) {
                return Err(PiServerError::new(
                    pi_protocol::ProtocolErrorCode::SessionLocked,
                    format!("Session is locked: {id}"),
                ));
            }
            inner.locked.insert(id.to_string());
            (stored, self.inner.clone())
        };
        let id_owned = id.to_string();
        let on_dispose = Arc::new(move || {
            inner_ref
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .locked
                .remove(&id_owned);
        });
        let runtime = TestSessionRuntime::new(stored, on_dispose);
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtimes
            .entry(id.to_string())
            .or_default()
            .push(runtime.clone());
        Ok(Arc::new(Mutex::new(runtime)))
    }
}

impl Default for TestServerService {
    fn default() -> Self {
        Self::new()
    }
}

impl PiServerService for TestServerService {
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiServerError> {
        let sessions = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        Ok(sessions
            .iter()
            .map(|snapshot| {
                metadata_of(&snapshot.lock().unwrap_or_else(|error| error.into_inner()))
            })
            .collect())
    }

    fn list_models(&self) -> Result<Vec<ModelMetadata>, PiServerError> {
        Ok(self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .models
            .clone())
    }

    fn create_session(
        &mut self,
        options: crate::types::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            if inner.sessions.contains_key(&options.id) {
                return Err(PiServerError::new(
                    pi_protocol::ProtocolErrorCode::SessionLocked,
                    "Session already exists",
                ));
            }
            inner.last_created_id = Some(options.id.clone());
            let now = 1;
            inner.sessions.insert(
                options.id.clone(),
                Arc::new(Mutex::new(SessionSnapshot {
                    id: options.id.clone(),
                    name: options.name,
                    cwd: options
                        .cwd
                        .unwrap_or_else(|| "/tmp/pi-server-conformance".to_string()),
                    created_at: now,
                    updated_at: now,
                    phase: SessionPhase::Idle,
                    model: options.model.unwrap_or(ModelRef {
                        provider: "test".to_string(),
                        id: "small".to_string(),
                    }),
                    thinking_level: options.thinking_level.unwrap_or(ThinkingLevel::Off),
                    attached: false,
                    locked: false,
                    revision: 0,
                    transcript: Vec::new(),
                    queued_steer: Vec::new(),
                    queued_steer_count: 0,
                })),
            );
        }
        self.acquire(&options.id)
    }

    fn open_session(
        &mut self,
        session_id: String,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        self.acquire(&session_id)
    }
}

// ---------------------------------------------------------------------------
// Command execution helpers (shared by LiveSessionManager)
// ---------------------------------------------------------------------------

/// Execute a command against the service and produce the protocol result.
pub fn session_snapshot_for_create(result: &CommandResult) -> Option<SessionSnapshot> {
    match result {
        CommandResult::Create { session }
        | CommandResult::Attach { session }
        | CommandResult::Prompt { session }
        | CommandResult::Steer { session }
        | CommandResult::Abort { session }
        | CommandResult::SetModel { session }
        | CommandResult::SetThinking { session } => Some(session.clone()),
        _ => None,
    }
}
