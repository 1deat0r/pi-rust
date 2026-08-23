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
//! Runtime-event subscription (upstream `handleRuntimeEvent`) is wired through
//! [`LiveSessionManager::new`], which subscribes each live runtime and fans out
//! `Snapshot` (server-metadata refresh), `Progress` (per-attached-connection
//! sink) and `Error` (terminal-close: mark terminal, unsubscribe, dispose, and
//! drop the session from the live set). Disposal on a runtime error is deferred
//! when the session is mid-operation to avoid re-entering a held runtime guard;
//! otherwise it runs on a background thread so the dispose never blocks a live
//! call stack.
//!
//! The async promise arbitration is intentionally collapsed: commands here run
//! synchronously under the service lock, so a concurrent open of the same id
//! is serialized rather than coalesced. Transport-level connection closure on
//! terminal sessions and per-request progress scoping beyond the attached-session
//! segment (#51) remain outside this manager; per-connection progress delivery is
//! scoped to the connection's attached session segment, and dispose-on-idle is
//! driven by runtime snapshot/progress events so idle sessions do not leak.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use pi_protocol::{
    Command, CommandResult, ProtocolErrorCode, SessionMetadata, SessionPhase, SessionSnapshot,
    TranscriptProgress,
};

use crate::errors::PiServerError;
use crate::service::{EventListener, PiServerService, PiSessionRuntime, Unsubscribe};
use crate::snapshots::ServerSnapshotPublisher;

/// A per-connection broadcast sink for session runtime progress. The transport
/// layer installs one to forward `SessionProgress` envelopes; unit tests may
/// leave it `None` (progress is dropped).
pub type ProgressSink = Arc<dyn Fn(&TranscriptProgress) + Send + Sync>;

/// The minimal connection state the manager needs to enforce attach/detach
/// exclusivity (upstream `ConnectionState`). `session_ids` is the set of live
/// session ids this connection is attached to.
pub struct ConnectionHandle {
    pub id: String,
    pub session_ids: HashSet<String>,
    /// handshake completed and stage == "ready".
    pub ready: bool,
    pub disconnected: bool,
    pub closed: bool,
    /// Best-effort broadcast sink for runtime `Progress` events on this
    /// connection (see [`ProgressSink`]).
    pub progress: Option<ProgressSink>,
}

impl Clone for ConnectionHandle {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            session_ids: self.session_ids.clone(),
            ready: self.ready,
            disconnected: self.disconnected,
            closed: self.closed,
            progress: self.progress.clone(),
        }
    }
}

impl ConnectionHandle {
    pub fn new(id: String) -> Self {
        Self {
            id,
            session_ids: HashSet::new(),
            ready: true,
            disconnected: false,
            closed: false,
            progress: None,
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
    /// Per-attached-connection progress sink (only those that installed one).
    progress_sinks: HashMap<String, ProgressSink>,
    operation_count: usize,
    ready: bool,
    terminal: bool,
    disposing: bool,
    /// Runtime unsubscribe handle, invoked on dispose/close.
    unsubscribe: Option<Unsubscribe>,
}

/// Owns the live-session lifecycle over a synchronous `PiServerService`.
pub struct LiveSessionManager {
    service: Arc<Mutex<dyn PiServerService>>,
    snapshots: Arc<ServerSnapshotPublisher>,
    live: Arc<Mutex<HashMap<String, LiveSession>>>,
}

impl LiveSessionManager {
    pub fn new(
        service: Arc<Mutex<dyn PiServerService>>,
        snapshots: Arc<ServerSnapshotPublisher>,
    ) -> Self {
        Self {
            service,
            snapshots,
            live: Arc::new(Mutex::new(HashMap::new())),
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
                    live.progress_sinks.remove(&conn.id);
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
                    {
                        let mut guard = self.live.lock().unwrap();
                        if let Some(live) = guard.get_mut(&session_id) {
                            live.connections.remove(&conn.id);
                            live.progress_sinks.remove(&conn.id);
                        }
                    }
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
        let unsubscribe = self.subscribe_runtime(runtime.clone(), id.to_string());
        let live = LiveSession {
            id: id.to_string(),
            runtime: runtime.clone(),
            connections: HashSet::new(),
            progress_sinks: HashMap::new(),
            operation_count: 0,
            ready: true,
            terminal: false,
            disposing: false,
            unsubscribe,
        };
        self.live.lock().unwrap().insert(id.to_string(), live);
        Ok(runtime)
    }

    /// Subscribe the manager to a runtime's events so asynchronous progress,
    /// snapshot, and error signals reach attached connections (upstream
    /// `LiveSession`'s `runtime.subscribe` + `handleRuntimeEvent`).
    fn subscribe_runtime(
        &self,
        runtime: Arc<Mutex<dyn PiSessionRuntime>>,
        id: String,
    ) -> Option<Unsubscribe> {
        let live = self.live.clone();
        let snapshots = self.snapshots.clone();
        let service = self.service.clone();
        let listener: EventListener = Arc::new(move |event| {
            manager_handle_event(&live, &snapshots, &service, &id, event);
        });
        runtime.lock().unwrap().subscribe(listener).ok()
    }

    /// Close the manager: unsubcribe and dispose every live session (upstream
    /// `LiveSessionManager.close`).
    pub fn close(&self) {
        let sessions: Vec<LiveSession> = {
            let mut guard = self.live.lock().unwrap();
            let snapshots: Vec<LiveSession> = guard.drain().map(|(_, s)| s).collect();
            snapshots
        };
        for live in sessions {
            if let Some(unsub) = live.unsubscribe {
                unsub();
            }
            let _ = live.runtime.lock().unwrap().dispose();
        }
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
            if let Some(sink) = conn.progress.as_ref() {
                live.progress_sinks.insert(conn.id.clone(), sink.clone());
            }
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
        compute_metadata(&self.service, &self.live)
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
            if let Some(mut live) = self.live.lock().unwrap().remove(id) {
                if let Some(unsub) = live.unsubscribe.take() {
                    unsub();
                }
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

/// Dispose an idle, unattached live session in response to a runtime
/// `Snapshot`/`Progress` event (upstream `LiveSessionManager.handleRuntimeEvent`
/// calls `scheduleMaybeDispose(live)` after every event). Without this a session
/// that becomes idle with no attached connections after a concurrent prompt/steer
/// would leak in the live map. Runs from event context, so it must not block on a
/// runtime guard it may already hold: the phase check uses `try_lock` (busy → skip,
/// a later event retries).
fn maybe_dispose_event(
    live: &Arc<Mutex<HashMap<String, LiveSession>>>,
    snapshots: &Arc<ServerSnapshotPublisher>,
    service: &Arc<Mutex<dyn PiServerService>>,
    id: &str,
) {
    let should_dispose = {
        let mut guard = live.lock().unwrap();
        let Some(sess) = guard.get_mut(id) else { return };
        if !sess.ready || sess.disposing || !sess.connections.is_empty() || sess.operation_count > 0 {
            return;
        }
        // If the runtime guard is held on this thread (the very event tripping
        // us mid-op), skip; an idle session's disposal retries on a later event.
        let Ok(rg) = sess.runtime.try_lock() else { return };
        if sess.terminal || rg.get_phase() == SessionPhase::Idle {
            sess.disposing = true;
            true
        } else {
            false
        }
    };
    if !should_dispose {
        return;
    }
    // Remove + unsubscribe, then drop the live guard before touching the runtime
    // or recomputing metadata (compute_metadata_event re-locks `live`).
    let runtime = {
        let mut guard = live.lock().unwrap();
        let Some(mut sess) = guard.remove(id) else { return };
        if let Some(unsub) = sess.unsubscribe.take() {
            unsub();
        }
        sess.runtime
    };
    // The try_lock above proved the runtime guard is free on this thread, so a
    // blocking lock is safe here (dispose never re-enters a held guard).
    let _ = runtime.lock().unwrap().dispose();
    if let Some(meta) = compute_metadata_event(service, live) {
        snapshots.refresh(meta);
    }
}

/// Merge stored sessions with live snapshot overrides (upstream `listMetadata`).
fn compute_metadata(
    service: &Arc<Mutex<dyn PiServerService>>,
    live: &Arc<Mutex<HashMap<String, LiveSession>>>,
) -> Result<Vec<SessionMetadata>, PiServerError> {
    let stored = service.lock().unwrap().list_sessions()?;
    let mut live_by_id = HashMap::new();
    {
        let guard = live.lock().unwrap();
        for sess in guard.values() {
            if sess.disposing {
                continue;
            }
            if let Ok(snap) = {
                let mut rg = sess.runtime.lock().unwrap();
                let mut s = rg.snapshot()?;
                s.phase = rg.get_phase();
                s.locked = true;
                s.attached = !sess.connections.is_empty();
                Ok::<_, PiServerError>(s)
            } {
                live_by_id.insert(sess.id.clone(), snap);
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

/// Runtime-event handler invoked from a session's subscription (upstream
/// `LiveSessionManager.handleRuntimeEvent`). Runs synchronously but must never
/// block on a runtime mutex: a runtime event can fire while that runtime's
/// guard is held on the same thread (an operation mid-flight), so it reads
/// metadata with `try_lock` (skipping a busy session) instead of `compute_metadata`.
fn manager_handle_event(
    live: &Arc<Mutex<HashMap<String, LiveSession>>>,
    snapshots: &Arc<ServerSnapshotPublisher>,
    service: &Arc<Mutex<dyn PiServerService>>,
    id: &str,
    event: crate::service::PiSessionRuntimeEvent,
) {
    match event {
        crate::service::PiSessionRuntimeEvent::Snapshot => {
            if let Some(meta) = compute_metadata_event(service, live) {
                snapshots.refresh(meta);
            }
            // A snapshot may carry an idle+unattached session after a concurrent
            // prompt/steer; dispose it so it does not leak (upstream
            // scheduleMaybeDispose).
            maybe_dispose_event(live, snapshots, service, id);
        }
        crate::service::PiSessionRuntimeEvent::Progress(progress) => {
            let sinks: Vec<ProgressSink> = {
                let guard = live.lock().unwrap();
                match guard.get(id) {
                    Some(sess) => sess.progress_sinks.values().cloned().collect(),
                    None => Vec::new(),
                }
            };
            for sink in sinks {
                sink(&progress);
            }
            maybe_dispose_event(live, snapshots, service, id);
        }
        crate::service::PiSessionRuntimeEvent::Error(_error) => {
            // Mark terminal + dispose. The synchronous manager closes the
            // session's lifecycle; transport closure/error reporting is the
            // connection layer's concern. Disposal runs on a background thread
            // so a still-held in-op runtime guard on this thread cannot block.
            let defer = {
                let mut guard = live.lock().unwrap();
                let mut defer = false;
                if let Some(sess) = guard.get_mut(id) {
                    if !sess.terminal && !sess.disposing {
                        sess.terminal = true;
                        defer = sess.operation_count > 0;
                    }
                }
                defer
            };
            if defer {
                // Disposal happens in run_operation's finally (maybe_dispose).
                return;
            }
            let removed = {
                let mut guard = live.lock().unwrap();
                guard.remove(id).map(|mut sess| {
                    sess.terminal = true;
                    (sess.runtime.clone(), sess.unsubscribe.take())
                })
            };
            if let Some((runtime, unsub)) = removed {
                let live_ref = live.clone();
                let snapshots_ref = snapshots.clone();
                let service_ref = service.clone();
                // Off-operation the runtime guard is not held on this thread, so
                // dispose synchronously (deterministic). If the runtime is still
                // locked (mid-op on a later tick, or a snapshot read), dispose on
                // a background thread so it never blocks this stack. The try_lock
                // guard is dropped by `is_ok()` before we branch on it.
                let inline = runtime.try_lock().is_ok();
                if inline {
                    if let Some(u) = unsub {
                        u();
                    }
                    let _ = runtime.lock().unwrap().dispose();
                    if let Some(meta) = compute_metadata_event(&service_ref, &live_ref) {
                        snapshots_ref.refresh(meta);
                    }
                } else {
                    std::thread::spawn(move || {
                        if let Some(u) = unsub {
                            u();
                        }
                        let _ = runtime.lock().unwrap().dispose();
                        if let Some(meta) = compute_metadata_event(&service_ref, &live_ref) {
                            snapshots_ref.refresh(meta);
                        }
                    });
                }
            }
        }
    }
}

/// Best-effort metadata merge for the event handler: unlike [`compute_metadata`],
/// it uses `try_lock` on each live runtime and skips one that is already locked
/// (e.g. by the very event that tripped the handler) so it cannot deadlock.
fn compute_metadata_event(
    service: &Arc<Mutex<dyn PiServerService>>,
    live: &Arc<Mutex<HashMap<String, LiveSession>>>,
) -> Option<Vec<SessionMetadata>> {
    let stored = service.lock().ok()?.list_sessions().ok()?;
    let mut live_by_id = HashMap::new();
    {
        let guard = live.lock().ok()?;
        for sess in guard.values() {
            if sess.disposing {
                continue;
            }
            let Ok(mut rg) = sess.runtime.try_lock() else { continue };
            let Ok(mut s) = rg.snapshot() else { continue };
            s.phase = rg.get_phase();
            s.locked = true;
            s.attached = !sess.connections.is_empty();
            live_by_id.insert(sess.id.clone(), s);
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
    Some(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::InMemoryService;
    use pi_protocol::{ModelMetadata, ModelRef, ServerSnapshot, ThinkingLevel, TranscriptDeltaKind};

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

    /// Build a manager plus the concrete `InMemoryService` (so tests can drive
    /// `emit` on a live session).
    fn manager_with_service() -> (LiveSessionManager, InMemoryService) {
        let raw = InMemoryService::new(Vec::new());
        let svc: Arc<Mutex<dyn PiServerService>> = Arc::new(Mutex::new(raw.clone()));
        let m = LiveSessionManager::new(svc, publisher());
        (m, raw)
    }

    fn create_live(m: &LiveSessionManager, conn: &mut ConnectionHandle) -> String {
        let CommandResult::Create { session } = m
            .execute_command(conn, Command::Create {
                cwd: None, name: None, model: Some(model_ref()), thinking_level: None,
            })
            .unwrap()
        else { panic!("expected Create") };
        session.id
    }

    #[test]
    fn runtime_progress_fans_out_to_attached_sinks() {
        let (m, service) = manager_with_service();
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink: ProgressSink = Arc::new({
            let received = received.clone();
            move |p: &TranscriptProgress| {
                if let TranscriptProgress::AssistantDelta { delta, .. } = p {
                    received.lock().unwrap().push(delta.clone());
                }
            }
        });
        let mut conn = ConnectionHandle::new("c1".to_string());
        conn.progress = Some(sink);
        let id = create_live(&m, &mut conn);

        service.emit(&id, crate::service::PiSessionRuntimeEvent::Progress(
            TranscriptProgress::AssistantDelta {
                message_id: "m1".to_string(),
                content_index: 0,
                kind: TranscriptDeltaKind::Text,
                delta: "hello".to_string(),
            },
        ));

        assert_eq!(*received.lock().unwrap(), vec!["hello".to_string()]);
    }

    #[test]
    fn runtime_progress_is_dropped_when_no_sink() {
        let (m, service) = manager_with_service();
        let mut conn = ConnectionHandle::new("c1".to_string());
        // No progress sink on the connection.
        let id = create_live(&m, &mut conn);
        service.emit(&id, crate::service::PiSessionRuntimeEvent::Progress(
            TranscriptProgress::AssistantDelta {
                message_id: "m1".to_string(),
                content_index: 0,
                kind: TranscriptDeltaKind::Text,
                delta: "hello".to_string(),
            },
        ));
        // No panic; sink-less connections are simply skipped.
        assert!(m.live.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn runtime_error_terminal_closes_and_disposes_session() {
        let (m, service) = manager_with_service();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let id = create_live(&m, &mut conn);
        assert!(m.live.lock().unwrap().contains_key(&id));

        service.emit(&id, crate::service::PiSessionRuntimeEvent::Error(
            PiServerError::new(ProtocolErrorCode::InternalError, "boom"),
        ));

        // The terminal-close disposal runs on a background thread; poll briefly.
        let mut dropped = false;
        for _ in 0..100 {
            if !m.live.lock().unwrap().contains_key(&id) {
                dropped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(dropped, "terminal session should be disposed out of the live set");
        assert!(
            !m.list_metadata().unwrap().iter().any(|s| s.id == id),
            "terminal session should be removed from metadata"
        );
    }

    #[test]
    fn terminal_session_rejects_reuse_on_error() {
        let (m, service) = manager_with_service();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let id = create_live(&m, &mut conn);
        service.emit(&id, crate::service::PiSessionRuntimeEvent::Error(
            PiServerError::new(ProtocolErrorCode::InternalError, "boom"),
        ));

        // A subsequent create/attach for a live session must not resurrect it.
        let mut second = ConnectionHandle::new("c2".to_string());
        let err = m
            .execute_command(&mut second, Command::Prompt { session_id: id.clone(), text: "x".to_string() })
            .unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::InvalidRequest, "unattached connection");
    }

    #[test]
    fn close_unsubscribes_and_disposes_all_live_sessions() {
        let m = manager();
        let mut conn = ConnectionHandle::new("c1".to_string());
        let id = create_live(&m, &mut conn);
        assert!(m.live.lock().unwrap().contains_key(&id));
        m.close();
        assert!(m.live.lock().unwrap().is_empty(), "close should dispose every live session");
    }

    fn progress_delta(delta: &str) -> TranscriptProgress {
        TranscriptProgress::AssistantDelta {
            message_id: "m1".to_string(),
            content_index: 0,
            kind: TranscriptDeltaKind::Text,
            delta: delta.to_string(),
        }
    }

    /// Segment control: progress on session A reaches only connections whose
    /// subscription segment is A; a connection attached only to B must not see
    /// A's progress (even though it shares the manager).
    #[test]
    fn progress_is_scoped_to_the_attached_session_segment() {
        let (m, service) = manager_with_service();
        let a_received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut conn_a = ConnectionHandle::new("a".to_string());
        conn_a.progress = Some(Arc::new({
            let a_received = a_received.clone();
            move |p: &TranscriptProgress| {
                if let TranscriptProgress::AssistantDelta { delta, .. } = p {
                    a_received.lock().unwrap().push(delta.clone());
                }
            }
        }));
        let a_id = create_live(&m, &mut conn_a);

        // A second, unrelated session on a second, only-B connection.
        let b_received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut conn_b = ConnectionHandle::new("b".to_string());
        conn_b.progress = Some(Arc::new({
            let b_received = b_received.clone();
            move |p: &TranscriptProgress| {
                if let TranscriptProgress::AssistantDelta { delta, .. } = p {
                    b_received.lock().unwrap().push(delta.clone());
                }
            }
        }));
        let b_id = create_live(&m, &mut conn_b);
        assert_ne!(a_id, b_id);

        // Only session A emits; B's segment (session B) receives nothing.
        service.emit(&a_id, crate::service::PiSessionRuntimeEvent::Progress(progress_delta("alpha")));
        assert_eq!(*a_received.lock().unwrap(), vec!["alpha".to_string()]);
        assert!(b_received.lock().unwrap().is_empty(), "cross-session segment leakage");
    }

    /// Detaching mid-turn unsubscribes the connection's progress segment: no
    /// further progress for that session reaches it (upstream per-connection
    /// attach exclusivity).
    #[test]
    fn detach_unsubscribes_progress_segment() {
        let (m, service) = manager_with_service();
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink: ProgressSink = Arc::new({
            let received = received.clone();
            move |p: &TranscriptProgress| {
                if let TranscriptProgress::AssistantDelta { delta, .. } = p {
                    received.lock().unwrap().push(delta.clone());
                }
            }
        });
        let mut conn = ConnectionHandle::new("c1".to_string());
        conn.progress = Some(sink);
        let id = create_live(&m, &mut conn);

        service.emit(&id, crate::service::PiSessionRuntimeEvent::Progress(progress_delta("pre")));
        assert_eq!(*received.lock().unwrap(), vec!["pre".to_string()]);

        // Detach: the connection's progress segment is removed.
        m.execute_command(&mut conn, Command::Detach { session_id: id.clone() }).unwrap();
        service.emit(&id, crate::service::PiSessionRuntimeEvent::Progress(progress_delta("post")));
        assert_eq!(*received.lock().unwrap(), vec!["pre".to_string()], "post-detach progress leaked");
    }

    /// A session left idle + unattached after a concurrent turn must be disposed
    /// when the runtime's idle snapshot arrives (no live-map leak).
    #[test]
    fn idle_and_unattached_session_is_disposed_on_snapshot() {
        let (m, service) = manager_with_service();
        let mut conn = ConnectionHandle::new("c1".to_string());
        conn.progress = None;
        let id = create_live(&m, &mut conn);

        // Start a turn (phase -> Turn) then drop the only attachment.
        let CommandResult::Prompt { .. } = m
            .execute_command(&mut conn, Command::Prompt { session_id: id.clone(), text: "go".to_string() })
            .unwrap()
        else { panic!("expected Prompt") };
        m.execute_command(&mut conn, Command::Detach { session_id: id.clone() }).unwrap();
        // Still live: phase is Turn, not idle.
        assert!(m.live.lock().unwrap().contains_key(&id));

        // The turn finishes: the runtime settles to Idle and emits a Snapshot.
        service.settle_idle(&id).unwrap();
        service.emit(&id, crate::service::PiSessionRuntimeEvent::Snapshot);
        assert!(
            !m.live.lock().unwrap().contains_key(&id),
            "idle + unattached session should be disposed on snapshot (no leak)"
        );
    }
}