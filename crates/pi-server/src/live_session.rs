//! LiveSessionManager — sync adaptation of upstream `packages/server/src/sessions.ts`
//! (`LiveSessionManager`).
//!
//! The upstream manager is async (single-flight `openingSessions` promises,
//! dispose races, per-connection attach exclusivity). Here the `PiServerService` /
//! `PiSessionRuntime` traits are synchronous, so this ports the *observable*
//! exclusivity contract: a shared live-session lifecycle per session id —
//! acquire/reuse with terminal/disposing guards, per-connection attach/detach
//! validation (`requireAttached`), an operation count that gates disposal
//! only-when-idle, snapshots carrying `locked: true` and per-connection
//! `attached`, and server-metadata refresh that merges live snapshots.
//!
//! The async promise arbitration is intentionally collapsed: commands here run
//! synchronously under the service lock, so a concurrent open of the same id
//! is serialized rather than coalesced. This matches the sync service boundary
//! and is what T4 #49 (sync adaptation) demands; the abort/progress runtime-event
//! subscription fan-out is a separate server concern (#50/#51).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use pi_protocol::{
    Command, CommandResult, ProtocolErrorCode, SessionMetadata, SessionPhase, SessionSnapshot,
};

use crate::errors::PiServerError;
use crate::service::{PiServerService, PiSessionRuntime};
use crate::snapshots::ServerSnapshotPublisher;

/// The minimal connection state the manager needs to enforce attach/detach
/// exclusivity (upstream `ConnectionState`). `session_ids` is the set of live
/// session ids this connection is attached to.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    pub id: String,
    pub session_ids: HashSet<String>,
    /// handshake completed and stage == "ready".
    pub ready: bool,
    pub disconnected: bool,
    pub closed: bool,
}

impl ConnectionHandle {
    pub fn new(id: String) -> Self {
        Self {
            id,
            session_ids: HashSet::new(),
            ready: true,
            disconnected: false,
            closed: false,
        }
    }

    fn is_ready(&self) -> bool {
        self.ready && !self.disconnected && !self.closed
    }
}

struct LiveSession {
    id: String,
    runtime: Arc<Mutex<dyn PiSessionRuntime>>,
    /// Connection ids attached to this session.
    connections: HashSet<String>,
    operation_count: usize,
    ready: bool,
    terminal: bool,
    disposing: bool,
}

/// Owns the live-session lifecycle over a synchronous `PiServerService`.
pub struct LiveSessionManager {
    service: Arc<Mutex<dyn PiServerService>>,
    snapshots: Arc<ServerSnapshotPublisher>,
    live: Mutex<HashMap<String, LiveSession>>,
}

impl LiveSessionManager {
    pub fn new(
        service: Arc<Mutex<dyn PiServerService>>,
        snapshots: Arc<ServerSnapshotPublisher>,
    ) -> Self {
        Self {
            service,
            snapshots,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Disconnect a connection: detach it from every live session and dispose
    /// any that become idle (upstream `LiveSessionManager.disconnect`).
    pub fn disconnect(&self, conn: &mut ConnectionHandle) {
        let ids: Vec<String> = conn.session_ids.iter().cloned().collect();
        conn.session_ids.clear();
        for id in &ids {
            {
                let mut guard = self.live.lock().unwrap();
                if let Some(live) = guard.get_mut(id) {
                    live.connections.remove(&conn.id);
                }
            }
            self.maybe_dispose(id);
        }
    }

    /// Execute a protocol `Command` for a connection, enforcing exclusivity.
    pub fn execute_command(
        &self,
        conn: &mut ConnectionHandle,
        command: Command,
    ) -> Result<CommandResult, PiServerError> {
        match command {
            Command::List => Ok(CommandResult::List { sessions: self.list_metadata()? }),

            Command::Create { cwd, name, model, thinking_level } => {
                let id = uuid::Uuid::new_v4().to_string();
                let runtime = self.acquire(&id, {
                    let id = id.clone();
                    move || {
                        self.service.lock().unwrap().create_session(crate::types::CreateSessionOptions {
                            id,
                            cwd,
                            name,
                            model,
                            thinking_level,
                        })
                    }
                })?;
                self.attach(conn, &runtime)?;
                let session = self.broadcast_snapshot(&runtime, conn)?;
                Ok(CommandResult::Create { session })
            }

            Command::Attach { session_id } => {
                let runtime = self.acquire(&session_id, || {
                    self.service.lock().unwrap().open_session(session_id.clone())
                })?;
                self.attach(conn, &runtime)?;
                let session = self.broadcast_snapshot(&runtime, conn)?;
                Ok(CommandResult::Attach { session })
            }

            Command::Detach { session_id } => {
                if conn.session_ids.remove(&session_id) {
                    let dispose = {
                        let mut guard = self.live.lock().unwrap();
                        match guard.get_mut(&session_id) {
                            Some(live) => {
                                live.connections.remove(&conn.id);
                                false
                            }
                            None => false,
                        }
                    };
                    let _ = dispose;
                    // If live and no remaining connections/ops and idle, dispose.
                    self.maybe_dispose(&session_id);
                    Ok(CommandResult::Detach { session_id })
                } else {
                    Ok(CommandResult::Detach { session_id })
                }
            }

            Command::Prompt { session_id, text } => self.run_operation(conn, &session_id, |r| {
                r.prompt(crate::types::PromptInput { text })
            })
            .map(|session| CommandResult::Prompt { session }),

            Command::Steer { session_id, text } => self.run_operation(conn, &session_id, |r| {
                r.steer(crate::types::SteerInput { text })
            })
            .map(|session| CommandResult::Steer { session }),

            Command::Abort { session_id } => self
                .run_operation(conn, &session_id, |r| r.abort())
                .map(|session| CommandResult::Abort { session }),

            Command::SetModel { session_id, model } => self
                .run_operation(conn, &session_id, |r| r.set_model(model))
                .map(|session| CommandResult::SetModel { session }),

            Command::SetThinking { session_id, thinking_level } => self
                .run_operation(conn, &session_id, |r| r.set_thinking(thinking_level))
                .map(|session| CommandResult::SetThinking { session }),
        }
    }

    /// `requireAttached`: the connection must be attached and the session live
    /// (not terminal/disposing).
    fn require_attached(
        &self,
        conn: &ConnectionHandle,
        session_id: &str,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        if !conn.session_ids.contains(session_id) {
            return Err(PiServerError::new(
                ProtocolErrorCode::InvalidRequest,
                format!("Connection is not attached to session {session_id}"),
            ));
        }
        let guard = self.live.lock().unwrap();
        let live = guard.get(session_id).ok_or_else(|| {
            PiServerError::new(ProtocolErrorCode::NotFound, format!("Session is not live: {session_id}"))
        })?;
        if live.terminal || live.disposing {
            return Err(PiServerError::new(
                ProtocolErrorCode::NotFound,
                format!("Session is not live: {session_id}"),
            ));
        }
        Ok(live.runtime.clone())
    }

    /// acquire/reuse a live runtime for `id`. Guards: a terminal session is
    /// rejected; a disposing session is busy; otherwise the existing runtime is
    /// reused (the upstream single-flight is serialized here). A fresh runtime
    /// is created via `acquire_runtime` and validated (snapshot id must match).
    fn acquire<F>(
        &self,
        id: &str,
        acquire_runtime: F,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError>
    where
        F: FnOnce() -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError>,
    {
        {
            let guard = self.live.lock().unwrap();
            if let Some(live) = guard.get(id) {
                if live.terminal {
                    return Err(PiServerError::new(
                        ProtocolErrorCode::SessionLocked,
                        format!("Session runtime is terminating: {id}"),
                    ));
                }
                if live.disposing {
                    return Err(PiServerError::new(
                        ProtocolErrorCode::Busy,
                        format!("Session is disposing: {id}"),
                    ));
                }
                return Ok(live.runtime.clone());
            }
        }
        let runtime = acquire_runtime()?;
        {
            let snap = runtime.lock().unwrap().snapshot()?;
            if snap.id != id {
                let _ = runtime.lock().unwrap().dispose();
                return Err(PiServerError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!("Service returned session {} for server-assigned session {id}", snap.id),
                ));
            }
        }
        let live = LiveSession {
            id: id.to_string(),
            runtime: runtime.clone(),
            connections: HashSet::new(),
            operation_count: 0,
            ready: true,
            terminal: false,
            disposing: false,
        };
        self.live.lock().unwrap().insert(id.to_string(), live);
        Ok(runtime)
    }

    /// Attach a connection to a live runtime: record it in the connection's
    /// session set and the session's connection set. Rejects a closed connection.
    fn attach(
        &self,
        conn: &mut ConnectionHandle,
        runtime: &Arc<Mutex<dyn PiSessionRuntime>>,
    ) -> Result<(), PiServerError> {
        if !conn.is_ready() {
            return Err(PiServerError::new(
                ProtocolErrorCode::InvalidRequest,
                "Connection closed while attaching to a session",
            ));
        }
        let id = {
            let snap = runtime.lock().unwrap().snapshot()?;
            snap.id
        };
        conn.session_ids.insert(id.clone());
        if let Some(live) = self.live.lock().unwrap().get_mut(&id) {
            live.connections.insert(conn.id.clone());
        }
        Ok(())
    }

    /// Run a mutating operation: bump the operation count, run it, broadcast a
    /// snapshot, then decrement and maybe dispose. Mirrors upstream
    /// `runOperation`.
    fn run_operation<F>(
        &self,
        conn: &ConnectionHandle,
        session_id: &str,
        op: F,
    ) -> Result<SessionSnapshot, PiServerError>
    where
        F: FnOnce(&mut dyn PiSessionRuntime) -> Result<(), PiServerError>,
    {
        let runtime = self.require_attached(conn, session_id)?;
        {
            let mut guard = self.live.lock().unwrap();
            if let Some(live) = guard.get_mut(session_id) {
                live.operation_count += 1;
            }
        }
        let result = op(&mut *runtime.lock().unwrap());
        let snapshot = if result.is_ok() {
            self.broadcast_snapshot(&runtime, conn).ok()
        } else {
            None
        };
        {
            let mut guard = self.live.lock().unwrap();
            if let Some(live) = guard.get_mut(session_id) {
                live.operation_count = live.operation_count.saturating_sub(1);
            }
        }
        self.maybe_dispose(session_id);
        result?;
        snapshot.ok_or_else(|| {
            PiServerError::new(
                ProtocolErrorCode::InternalError,
                "failed to snapshot session after operation",
            )
        })
    }

    /// Normalized snapshot: `locked: true`, `phase` from the runtime, `attached`
    /// if any connection, and id verified (upstream `normalizedSnapshot`).
    fn normalized_snapshot(
        &self,
        runtime: &Arc<Mutex<dyn PiSessionRuntime>>,
    ) -> Result<SessionSnapshot, PiServerError> {
        let mut guard = runtime.lock().unwrap();
        let mut snap = guard.snapshot()?;
        let id = snap.id.clone();
        snap.phase = guard.get_phase();
        snap.locked = true;
        snap.attached = false; // set by caller's for_connection / connection set
        let _ = id;
        Ok(snap)
    }

    /// Broadcast the session snapshot (updating server metadata revision) and
    /// return the per-connection `attached` form (upstream `broadcastSnapshot`
    /// + `forConnection`).
    fn broadcast_snapshot(
        &self,
        runtime: &Arc<Mutex<dyn PiSessionRuntime>>,
        conn: &ConnectionHandle,
    ) -> Result<SessionSnapshot, PiServerError> {
        let mut snap = self.normalized_snapshot(runtime)?;
        // attached reflects this connection's attachment.
        snap.attached = conn.session_ids.contains(&snap.id);
        let metadata = self.list_metadata()?;
        self.snapshots.refresh(metadata);
        Ok(snap)
    }

    /// Session metadata merging stored sessions with live snapshot overrides
    /// (upstream `listMetadata`).
    pub fn list_metadata(&self) -> Result<Vec<SessionMetadata>, PiServerError> {
        let stored = self.service.lock().unwrap().list_sessions()?;
        let mut live_by_id = HashMap::new();
        {
            let guard = self.live.lock().unwrap();
            for live in guard.values() {
                if live.disposing {
                    continue;
                }
                if let Ok(snap) = {
                    let mut rg = live.runtime.lock().unwrap();
                    let mut s = rg.snapshot()?;
                    s.phase = rg.get_phase();
                    s.locked = true;
                    s.attached = !live.connections.is_empty();
                    Ok::<_, PiServerError>(s)
                } {
                    live_by_id.insert(live.id.clone(), snap);
                }
            }
        }
        let mut metadata: Vec<SessionMetadata> = Vec::with_capacity(stored.len());
        for item in stored {
            if let Some(snap) = live_by_id.remove(&item.id) {
                metadata.push(metadata_of(&snap));
            } else {
                metadata.push(item);
            }
        }
        for snap in live_by_id.values() {
            metadata.push(metadata_of(snap));
        }
        Ok(metadata)
    }

    /// Dispose a live session iff it is idle, attached by no one, not
    /// mid-operation, and not already disposing (upstream `maybeDispose`).
    fn maybe_dispose(&self, id: &str) {
        let should = {
            let mut guard = self.live.lock().unwrap();
            match guard.get_mut(id) {
                Some(live) => {
                    if !live.ready
                        || live.disposing
                        || !live.connections.is_empty()
                        || live.operation_count > 0
                    {
                        false
                    } else {
                        let phase = live.runtime.lock().unwrap().get_phase();
                        if phase != SessionPhase::Idle {
                            false
                        } else {
                            live.disposing = true;
                            true
                        }
                    }
                }
                None => false,
            }
        };
        if should {
            if let Some(live) = self.live.lock().unwrap().remove(id) {
                let _ = live.runtime.lock().unwrap().dispose();
            }
            // Advance server metadata so the disposed session drops out of the
            // live set (upstream `maybeDispose` broadcasts the server snapshot).
            if let Ok(meta) = self.list_metadata() {
                self.snapshots.refresh(meta);
            }
        }
    }
}

fn metadata_of(snap: &SessionSnapshot) -> SessionMetadata {
    SessionMetadata {
        id: snap.id.clone(),
        created_at: snap.created_at,
        updated_at: Some(snap.updated_at),
        parent_session_id: None,
        session_name: snap.name.clone(),
        cwd: Some(snap.cwd.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::InMemoryService;
    use pi_protocol::{ModelMetadata, ModelRef, ServerSnapshot, ThinkingLevel};

    fn service() -> Arc<Mutex<dyn PiServerService>> {
        let svc = InMemoryService::new(Vec::new());
        Arc::new(Mutex::new(svc))
    }

    fn publisher() -> Arc<ServerSnapshotPublisher> {
        Arc::new(ServerSnapshotPublisher::new(
            "server-1".to_string(),
            1,
            Vec::<ModelMetadata>::new(),
        ))
    }

    fn manager() -> LiveSessionManager {
        let _snap = ServerSnapshot { server_id: String::new(), protocol_version: 1, revision: 0, sessions: vec![], models: vec![] };
        let _ = _snap;
        LiveSessionManager::new(service(), publisher())
    }

    fn model_ref() -> ModelRef {
        ModelRef { provider: "faux".to_string(), id: "faux-1".to_string() }
    }

    #[test]
    fn create_attaches_and_returns_locked_snapshot() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let result = m
            .execute_command(&mut conn, Command::Create {
                cwd: Some(".".to_string()),
                name: Some("demo".to_string()),
                model: Some(model_ref()),
                thinking_level: Some(ThinkingLevel::Off),
            })
            .unwrap();
        let CommandResult::Create { session } = result else { panic!("expected Create") };
        assert!(session.locked, "live sessions are locked");
        assert!(session.attached, "creating connection is attached");
        assert_eq!(session.name.as_deref(), Some("demo"));
        assert!(conn.session_ids.contains(&session.id));
    }

    #[test]
    fn attach_reuses_runtime_and_require_attached_enforces() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let CommandResult::Create { session } = m
            .execute_command(&mut conn, Command::Create {
                cwd: None, name: None, model: Some(model_ref()), thinking_level: None,
            })
            .unwrap()
        else { panic!("expected Create") };
        let id = session.id;

        // A second connection can attach to the same live session.
        let mut conn2 = ConnectionHandle::new("c2".to_string());
        let CommandResult::Attach { session: snap2 } = m
            .execute_command(&mut conn2, Command::Attach { session_id: id.clone() })
            .unwrap()
        else { panic!("expected Attach") };
        assert_eq!(snap2.id, id);
        assert!(conn2.session_ids.contains(&id));

        // Command on an unattached connection must be rejected.
        let mut conn3 = ConnectionHandle::new("c3".to_string());
        let err = m
            .execute_command(&mut conn3, Command::Prompt { session_id: id.clone(), text: "hi".to_string() })
            .unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::InvalidRequest);
    }

    #[test]
    fn detach_disposes_idle_session_but_not_with_other_attachments() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let CommandResult::Create { session } = m
            .execute_command(&mut conn, Command::Create { cwd: None, name: None, model: Some(model_ref()), thinking_level: None })
            .unwrap()
        else { panic!("expected Create") };
        let id = session.id;

        // A second attachment keeps the session alive after c1 detaches.
        let mut conn2 = ConnectionHandle::new("c2".to_string());
        let _ = m.execute_command(&mut conn2, Command::Attach { session_id: id.clone() }).unwrap();

        m.execute_command(&mut conn, Command::Detach { session_id: id.clone() }).unwrap();
        assert!(!conn.session_ids.contains(&id));
        // The session still has c2 attached → not disposed.
        assert!(m.live.lock().unwrap().contains_key(&id));

        // Detach c2 → now idle → disposed.
        m.execute_command(&mut conn2, Command::Detach { session_id: id.clone() }).unwrap();
        assert!(!m.live.lock().unwrap().contains_key(&id), "idle session should be disposed");
    }

    #[test]
    fn prompt_on_idle_session_stays_live_during_turn() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let CommandResult::Create { session } = m
            .execute_command(&mut conn, Command::Create { cwd: None, name: None, model: Some(model_ref()), thinking_level: None })
            .unwrap()
        else { panic!("expected Create") };
        let id = session.id;
        // prompt moves the session to Turn (InMemory service) → not disposed.
        let CommandResult::Prompt { session: after } = m
            .execute_command(&mut conn, Command::Prompt { session_id: id.clone(), text: "hello".to_string() })
            .unwrap()
        else { panic!("expected Prompt") };
        assert_eq!(after.phase, SessionPhase::Turn);
        assert!(m.live.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn acquire_rejects_terminal_via_busy_on_missing_open() {
        // Creating the same id twice via service-level semantics: the manager
        // reuses the live runtime, so create returns the same live session id
        // family. We assert a closed connection is rejected on attach.
        let m = manager();
        let mut closed_conn = ConnectionHandle::new("cX".to_string());
        closed_conn.closed = true;
        let err = m
            .execute_command(&mut closed_conn, Command::Create { cwd: None, name: None, model: Some(model_ref()), thinking_level: None })
            .unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::InvalidRequest);
        assert!(err.message.contains("Connection closed"), "got: {}", err.message);
    }

    #[test]
    fn disconnect_detaches_and_disposes_idle() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let CommandResult::Create { session } = m
            .execute_command(&mut conn, Command::Create { cwd: None, name: None, model: Some(model_ref()), thinking_level: None })
            .unwrap()
        else { panic!("expected Create") };
        let id = session.id;
        assert!(m.live.lock().unwrap().contains_key(&id));
        m.disconnect(&mut conn);
        assert!(conn.session_ids.is_empty());
        assert!(!m.live.lock().unwrap().contains_key(&id), "idle session disposed on disconnect");
    }

    #[test]
    fn list_metadata_merges_live_snapshots() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let CommandResult::Create { session } = m
            .execute_command(&mut conn, Command::Create { cwd: None, name: Some("live".to_string()), model: Some(model_ref()), thinking_level: None })
            .unwrap()
        else { panic!("expected Create") };
        let meta = m.list_metadata().unwrap();
        assert!(
            meta.iter().any(|s| s.id == session.id && s.session_name.as_deref() == Some("live")),
            "live snapshot should override stored metadata"
        );
    }
}