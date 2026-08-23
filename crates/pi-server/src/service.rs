//! Service boundary and in-memory test service — port of
//! `packages/server/src/types.ts` + `packages/server/src/testing/service.ts`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_protocol::{
    CommandResult, ModelMetadata, ModelRef, SessionMetadata, SessionPhase, SessionSnapshot,
    ThinkingLevel, TranscriptItem, TranscriptProgress, UserContent, UserTranscriptItem,
};

use crate::errors::PiServerError;

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
}

pub type EventListener = Arc<dyn Fn(PiSessionRuntimeEvent) + Send + Sync>;
pub type Unsubscribe = Box<dyn Fn() + Send + Sync>;

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
    listeners: BTreeMap<String, Vec<EventListener>>,
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

    fn snapshot_for(&self, session_id: &str, revision: i64) -> Result<SessionSnapshot, PiServerError> {
        let inner = self.inner.lock().unwrap();
        let snap = inner
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| PiServerError::new(pi_protocol::ProtocolErrorCode::NotFound, format!("Session not found: {session_id}")))?;
        let mut snap = snap;
        snap.revision = revision;
        Ok(snap)
    }

    /// Fire a runtime event at all live listeners for `session_id` (used to
    /// script progress/error fan-out and terminal-close in tests). Best-effort
    /// like the upstream test service: the listener registry is append-only.
    pub fn emit(&self, session_id: &str, event: PiSessionRuntimeEvent) {
        let listeners = {
            let inner = self.inner.lock().unwrap();
            inner.listeners.get(session_id).cloned().unwrap_or_default()
        };
        for listener in listeners {
            listener(event.clone());
        }
    }

    /// Settle a running session back to Idle and emit a `Snapshot` (used by
    /// tests to simulate a streaming turn finishing so the manager's
    /// dispose-on-idle logic can be exercised).
    pub fn settle_idle(&self, session_id: &str) -> Result<(), PiServerError> {
        let mut inner = self.inner.lock().unwrap();
        let snap = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| PiServerError::new(pi_protocol::ProtocolErrorCode::NotFound, "Session not found".to_string()))?;
        snap.phase = SessionPhase::Idle;
        Ok(())
    }
}

impl PiServerService for InMemoryService {
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiServerError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.sessions.values().map(metadata_of).collect())
    }
    fn list_models(&self) -> Result<Vec<ModelMetadata>, PiServerError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.models.clone())
    }
    fn create_session(
        &mut self,
        options: crate::types::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.sessions.contains_key(&options.id) {
            return Err(PiServerError::new(pi_protocol::ProtocolErrorCode::InvalidRequest, format!("Session already exists: {}", options.id)));
        }
        let now = pi_ai::types::now_ms() as i64;
        let model = options.model.clone().unwrap_or(ModelRef { provider: "faux".into(), id: "faux-1".into() });
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
            let inner = self.inner.lock().unwrap();
            if !inner.sessions.contains_key(&session_id) {
                return Err(PiServerError::new(pi_protocol::ProtocolErrorCode::NotFound, format!("Session not found: {session_id}")));
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
    fn new(service: InMemoryService, session_id: String) -> Arc<Mutex<dyn PiSessionRuntime>> {
        Arc::new(Mutex::new(Self { service, session_id }))
    }
}

impl PiSessionRuntime for TestSession {
    fn snapshot(&mut self) -> Result<SessionSnapshot, PiServerError> {
        self.service.snapshot_for(&self.session_id, 1)
    }
    fn get_phase(&self) -> SessionPhase {
        let inner = self.service.inner.lock().unwrap();
        inner
            .sessions
            .get(&self.session_id)
            .map(|s| s.phase.clone())
            .unwrap_or(SessionPhase::Idle)
    }
    fn prompt(&mut self, _input: crate::types::PromptInput) -> Result<(), PiServerError> {
        let id = uuid::Uuid::new_v4().to_string();
        let item = UserTranscriptItem {
            id,
            role: "user".to_string(),
            content: vec![UserContent::Text(pi_protocol::TextContent::Text { text: _input.text.clone() })],
            timestamp: pi_ai::types::now_ms() as i64,
        };
        self.update(|snap| {
            snap.transcript.push(TranscriptItem::User(item.clone()));
            snap.phase = SessionPhase::Turn;
        })?;
        Ok(())
    }
    fn steer(&mut self, _input: crate::types::SteerInput) -> Result<(), PiServerError> {
        let item = UserTranscriptItem {
            id: uuid::Uuid::new_v4().to_string(),
            role: "user".to_string(),
            content: vec![UserContent::Text(pi_protocol::TextContent::Text { text: _input.text.clone() })],
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
        self.update(|snap| {
            snap.phase = SessionPhase::Turn;
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
        let mut inner = self.service.inner.lock().unwrap();
        inner.listeners.entry(self.session_id.clone()).or_default().push(listener);
        let _session_id = self.session_id.clone();
        Ok(Box::new(move || {
            // Listener removal is a no-op for the in-memory service (the
            // registry is appended; best-effort like the upstream tests).
            let _ = _session_id;
        }))
    }
    fn dispose(&mut self) -> Result<(), PiServerError> {
        // Disposal removes the session from the store so it drops out of
        // `listSessions`/server metadata (upstream dispose semantics).
        self.service.inner.lock().unwrap().sessions.remove(&self.session_id);
        Ok(())
    }
}

impl TestSession {
    fn update(&self, f: impl FnOnce(&mut SessionSnapshot)) -> Result<(), PiServerError> {
        let mut inner = self.service.inner.lock().unwrap();
        let snap = inner
            .sessions
            .get_mut(&self.session_id)
            .ok_or_else(|| PiServerError::new(pi_protocol::ProtocolErrorCode::NotFound, "Session not found".to_string()))?;
        f(snap);
        snap.updated_at = pi_ai::types::now_ms() as i64;
        snap.revision += 1;
        let listeners = inner.listeners.get(&self.session_id).cloned().unwrap_or_default();
        drop(inner);
        for listener in listeners {
            listener(PiSessionRuntimeEvent::Snapshot);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Command execution helpers (shared by LiveSessionManager)
// ---------------------------------------------------------------------------

/// Execute a command against the service and produce the protocol result.
pub fn session_snapshot_for_create(result: &CommandResult) -> Option<SessionSnapshot> {
    match result {
        CommandResult::Create { session } | CommandResult::Attach { session } | CommandResult::Prompt { session } | CommandResult::Steer { session } | CommandResult::Abort { session } | CommandResult::SetModel { session } | CommandResult::SetThinking { session } => Some(session.clone()),
        _ => None,
    }
}
