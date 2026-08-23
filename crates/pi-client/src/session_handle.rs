//! Session handle — port of `packages/client/src/session-handle.ts`.
//!
//! A `SessionHandle` is a small lease object scoped to one session: it wraps
//! the protocol commands (`prompt`, `steer`, `abort`, `set_model`,
//! `set_thinking`) and exposes snapshot subscription + attach/detach, exactly
//! like upstream's `SessionHandle implements SessionLease`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_protocol::{Command, CommandResult, ModelRef, ServerEvent, ThinkingLevel};

use crate::{PiClient, PiClientError};

/// Lease mode for acquiring a session handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLeaseMode {
    Shared,
    Exclusive,
}

/// Options for `PiClient::start_session` / `acquire_session`.
#[derive(Debug, Clone, Copy)]
pub struct AcquireSessionOptions {
    pub mode: SessionLeaseMode,
}

impl Default for AcquireSessionOptions {
    fn default() -> Self {
        Self {
            mode: SessionLeaseMode::Shared,
        }
    }
}

/// Unsubscribe handle (upstream `Unsubscribe`).
pub type Unsubscribe = Box<dyn Fn() + Send + Sync>;

type SnapshotListener = Box<dyn Fn(&pi_protocol::SessionSnapshot) + Send + Sync>;
type ServerEventListener = Box<dyn Fn(&ServerEvent) + Send + Sync>;

/// A handle to one session on the connected server.
#[derive(Clone)]
pub struct SessionHandle {
    id: String,
    client: PiClient,
    attached: Arc<AtomicBool>,
    forwarder: Arc<dyn Fn() + Send + Sync>,
    snapshot_listeners: Arc<Mutex<Vec<Option<SnapshotListener>>>>,
    event_listeners: Arc<Mutex<Vec<Option<ServerEventListener>>>>,
    disposed: Arc<AtomicBool>,
}

impl SessionHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// True while the underlying session is attached (mirror of upstream `active`/`attached`).
    pub fn attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    pub fn active(&self) -> bool {
        self.attached()
    }

    /// Most recent session snapshot observed by the client (may be stale).
    pub fn snapshot(&self) -> Option<pi_protocol::SessionSnapshot> {
        self.client.session_snapshot(&self.id)
    }

    /// Subscribe to session snapshots. Returns an `Unsubscribe`.
    pub fn subscribe(
        &self,
        listener: impl Fn(&pi_protocol::SessionSnapshot) + Send + Sync + 'static,
    ) -> Unsubscribe {
        self.subscribe_boxed(Box::new(listener))
    }

    fn subscribe_boxed(&self, listener: SnapshotListener) -> Unsubscribe {
        let mut listeners = self.snapshot_listeners.lock().unwrap();
        listeners.push(Some(listener));
        let idx = listeners.len() - 1;
        let listeners = self.snapshot_listeners.clone();
        Box::new(move || {
            listeners.lock().unwrap()[idx] = None;
        })
    }

    /// Subscribe to raw server events funneled for this session.
    pub fn on_event(&self, listener: impl Fn(&ServerEvent) + Send + Sync + 'static) -> Unsubscribe {
        self.on_event_boxed(Box::new(listener))
    }

    fn on_event_boxed(&self, listener: ServerEventListener) -> Unsubscribe {
        let mut listeners = self.event_listeners.lock().unwrap();
        listeners.push(Some(listener));
        let idx = listeners.len() - 1;
        let listeners = self.event_listeners.clone();
        Box::new(move || {
            listeners.lock().unwrap()[idx] = None;
        })
    }

    async fn request(
        &self,
        command: Command,
    ) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        let result = self.client.request(command).await?;
        match result {
            CommandResult::Prompt { session }
            | CommandResult::Steer { session }
            | CommandResult::Abort { session }
            | CommandResult::SetModel { session }
            | CommandResult::SetThinking { session }
            | CommandResult::Attach { session } => Ok(session),
            CommandResult::Detach { .. }
            | CommandResult::Create { .. }
            | CommandResult::List { .. } => Err(PiClientError {
                message: "unexpected command result for session command".into(),
            }),
        }
    }

    /// Send a prompt and return the resulting session snapshot.
    pub async fn prompt(&self, text: &str) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        self.request(Command::Prompt {
            session_id: self.id.clone(),
            text: text.to_string(),
        })
        .await
    }

    pub async fn steer(&self, text: &str) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        self.request(Command::Steer {
            session_id: self.id.clone(),
            text: text.to_string(),
        })
        .await
    }

    pub async fn abort(&self) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        self.request(Command::Abort {
            session_id: self.id.clone(),
        })
        .await
    }

    pub async fn set_model(
        &self,
        model: ModelRef,
    ) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        self.request(Command::SetModel {
            session_id: self.id.clone(),
            model,
        })
        .await
    }

    pub async fn set_thinking(
        &self,
        thinking_level: ThinkingLevel,
    ) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        self.request(Command::SetThinking {
            session_id: self.id.clone(),
            thinking_level,
        })
        .await
    }

    /// Detach from the session (upstream `detach`).
    pub async fn detach(&self) -> Result<(), PiClientError> {
        let result = self
            .client
            .request(Command::Detach {
                session_id: self.id.clone(),
            })
            .await?;
        match result {
            CommandResult::Detach { session_id } => {
                if session_id == self.id {
                    self.attached.store(false, Ordering::SeqCst);
                    Ok(())
                } else {
                    Err(PiClientError {
                        message: format!("detach returned wrong session {session_id}"),
                    })
                }
            }
            _ => Err(PiClientError {
                message: "unexpected command result for detach".into(),
            }),
        }
    }

    /// Dispose the handle: unsubscribe all listeners and mark released.
    pub async fn dispose(&self) -> Result<(), PiClientError> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut listeners = self.snapshot_listeners.lock().unwrap();
            for slot in listeners.iter_mut() {
                *slot = None;
            }
        }
        {
            let mut listeners = self.event_listeners.lock().unwrap();
            for slot in listeners.iter_mut() {
                *slot = None;
            }
        }
        (self.forwarder)();
        self.attached.store(false, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PiClient surface
// ---------------------------------------------------------------------------

impl PiClient {
    /// Create a session and return an attached handle (`startSession`).
    pub async fn start_session(
        &self,
        cwd: Option<String>,
        name: Option<String>,
        model: Option<ModelRef>,
        thinking_level: Option<ThinkingLevel>,
        options: AcquireSessionOptions,
    ) -> Result<SessionHandle, PiClientError> {
        let result = self
            .request(Command::Create {
                cwd,
                name,
                model,
                thinking_level,
            })
            .await?;
        let session = match result {
            CommandResult::Create { session } => session,
            _ => {
                return Err(PiClientError {
                    message: "unexpected command result for create".into(),
                })
            }
        };
        self.attach_session(&session.id, options).await
    }

    /// Acquire a handle to an existing session (`acquireSession`).
    pub async fn acquire_session(
        &self,
        session_id: &str,
        options: AcquireSessionOptions,
    ) -> Result<SessionHandle, PiClientError> {
        self.attach_session(session_id, options).await
    }

    async fn attach_session(
        &self,
        session_id: &str,
        _options: AcquireSessionOptions,
    ) -> Result<SessionHandle, PiClientError> {
        let result = self
            .request(Command::Attach {
                session_id: session_id.to_string(),
            })
            .await?;
        let session = match result {
            CommandResult::Attach { session } if session.id == session_id => session,
            CommandResult::Attach { session } => {
                return Err(PiClientError {
                    message: format!("attach returned session {}", session.id),
                })
            }
            _ => {
                return Err(PiClientError {
                    message: "unexpected command result for attach".into(),
                })
            }
        };
        self.note_session_snapshot(session.clone());
        let attached = Arc::new(AtomicBool::new(true));
        let snapshot_listeners: Arc<Mutex<Vec<Option<SnapshotListener>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let event_listeners: Arc<Mutex<Vec<Option<ServerEventListener>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let live = Arc::new(AtomicBool::new(true));
        let sid = session_id.to_string();

        // One global client listener fans out this session's snapshots and
        // events to the handle's subscribers; gated by `live` so disposed
        // handles stop forwarding (the client prunes dead listeners).
        let global_live = live.clone();
        let snap = snapshot_listeners.clone();
        let evt = event_listeners.clone();
        self.subscribe(Arc::new(move |event: &ServerEvent| {
            if !global_live.load(Ordering::SeqCst) {
                return;
            }
            match event {
                ServerEvent::SessionSnapshot { snapshot } if snapshot.id == sid => {
                    let listeners = snap.lock().unwrap();
                    for some in listeners.iter().flatten() {
                        some(snapshot);
                    }
                }
                ServerEvent::SessionProgress {
                    session_id: eid, ..
                }
                | ServerEvent::SessionRemoved { session_id: eid }
                    if *eid == sid =>
                {
                    let listeners = evt.lock().unwrap();
                    for some in listeners.iter().flatten() {
                        some(event);
                    }
                }
                _ => {}
            }
        }));

        let forwarder: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            live.store(false, Ordering::SeqCst);
        });

        Ok(SessionHandle {
            id: session_id.to_string(),
            client: self.clone(),
            attached,
            forwarder,
            snapshot_listeners,
            event_listeners,
            disposed: Arc::new(AtomicBool::new(false)),
        })
    }
}
