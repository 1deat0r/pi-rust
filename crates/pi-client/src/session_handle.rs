//! Session leases and the command surface for one attached session.

use std::sync::{Arc, Mutex};

use pi_protocol::{Command, CommandResult, ModelRef, ServerEvent, ThinkingLevel};
use tokio::sync::Notify;

use crate::{ConnectionStateUnsubscribe, PiClient, PiClientError, SessionLeaseToken};

/// Lease mode for acquiring a session handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLeaseMode {
    Shared,
    Exclusive,
}

/// Options for acquiring an existing session or starting a new one.
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

/// A callback removal function.
pub type Unsubscribe = Box<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseStatus {
    Active,
    Releasing,
    Released,
    Invalidated,
}

struct ReleaseCompletion {
    result: Mutex<Option<Result<(), PiClientError>>>,
    notify: Notify,
}

impl ReleaseCompletion {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    fn complete(&self, result: Result<(), PiClientError>) {
        *self.result.lock().unwrap() = Some(result);
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> Result<(), PiClientError> {
        loop {
            let notified = self.notify.notified();
            if let Some(result) = self.result.lock().unwrap().clone() {
                return result;
            }
            notified.await;
        }
    }
}

struct Subscription {
    unsubscribe: Mutex<Option<ConnectionStateUnsubscribe>>,
}

impl Subscription {
    fn new(unsubscribe: ConnectionStateUnsubscribe) -> Arc<Self> {
        Arc::new(Self {
            unsubscribe: Mutex::new(Some(unsubscribe)),
        })
    }

    fn unsubscribe(&self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().unwrap().take() {
            unsubscribe();
        }
    }
}

/// A lease-backed handle to one session on the server.
#[derive(Clone)]
pub struct SessionHandle {
    id: String,
    client: PiClient,
    token: SessionLeaseToken,
    status: Arc<Mutex<LeaseStatus>>,
    release_completion: Arc<Mutex<Option<Arc<ReleaseCompletion>>>>,
    subscriptions: Arc<Mutex<Vec<Arc<Subscription>>>>,
}

impl SessionHandle {
    pub(crate) fn new(client: PiClient, token: SessionLeaseToken) -> Self {
        Self {
            id: token.session_id.clone(),
            client,
            token,
            status: Arc::new(Mutex::new(LeaseStatus::Active)),
            release_completion: Arc::new(Mutex::new(None)),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn attached(&self) -> bool {
        self.refresh_status();
        matches!(&*self.status.lock().unwrap(), LeaseStatus::Active)
            && self.client.is_session_attached(&self.id)
    }

    pub fn active(&self) -> bool {
        self.attached()
    }

    pub fn snapshot(&self) -> Option<pi_protocol::SessionSnapshot> {
        self.attached()
            .then(|| self.client.session_snapshot(&self.id))
            .flatten()
    }

    /// Subscribe to snapshots while this lease remains active.
    ///
    /// The historical Rust surface returns an unsubscribe callback rather than
    /// a `Result`; an inactive handle therefore returns a harmless no-op.
    pub fn subscribe(
        &self,
        listener: impl Fn(&pi_protocol::SessionSnapshot) + Send + Sync + 'static,
    ) -> Unsubscribe {
        if !self.is_active() {
            return Box::new(|| {});
        }
        let client = self.client.clone();
        let id = self.id.clone();
        let status = self.status.clone();
        let token = self.token.clone();
        let callback_client = client.clone();
        let subscription =
            Subscription::new(client.subscribe_session_snapshots(&id, move |snapshot| {
                if handle_is_active(&callback_client, &token, &status) {
                    listener(snapshot);
                }
            }));
        self.subscriptions
            .lock()
            .unwrap()
            .push(subscription.clone());
        Box::new(move || subscription.unsubscribe())
    }

    /// Subscribe to events associated with this session.
    pub fn on_event(&self, listener: impl Fn(&ServerEvent) + Send + Sync + 'static) -> Unsubscribe {
        if !self.is_active() {
            return Box::new(|| {});
        }
        let client = self.client.clone();
        let id = self.id.clone();
        let status = self.status.clone();
        let token = self.token.clone();
        let callback_client = client.clone();
        let subscription = Subscription::new(client.subscribe_session_events(&id, move |event| {
            if handle_is_active(&callback_client, &token, &status)
                || matches!(event, ServerEvent::SessionRemoved { .. })
            {
                listener(event);
            }
        }));
        self.subscriptions
            .lock()
            .unwrap()
            .push(subscription.clone());
        Box::new(move || subscription.unsubscribe())
    }

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

    async fn request(
        &self,
        command: Command,
    ) -> Result<pi_protocol::SessionSnapshot, PiClientError> {
        self.assert_active()?;
        let result = self.client.request(command).await?;
        match result {
            CommandResult::Prompt { session }
            | CommandResult::Steer { session }
            | CommandResult::Abort { session }
            | CommandResult::SetModel { session }
            | CommandResult::SetThinking { session }
            | CommandResult::Attach { session } => Ok(session),
            _ => Err(PiClientError {
                message: "unexpected command result for session command".into(),
            }),
        }
    }

    /// Explicitly detach this lease. A failed detach keeps the lease active so
    /// callers can retry; a successful final detach releases the server lease.
    pub async fn detach(&self) -> Result<(), PiClientError> {
        self.release(false).await
    }

    /// Dispose the handle and relinquish the lease even if cleanup fails.
    pub async fn dispose(&self) -> Result<(), PiClientError> {
        let result = self.release(true).await;
        if result.is_ok() || !self.is_active() {
            self.clear_subscriptions();
        }
        result
    }

    fn is_active(&self) -> bool {
        self.refresh_status();
        matches!(&*self.status.lock().unwrap(), LeaseStatus::Active)
            && self.client.is_connected()
            && self.client.is_session_attached(&self.id)
    }

    fn refresh_status(&self) {
        let current = self.client.lease_generation_is_current(&self.token);
        let mut status = self.status.lock().unwrap();
        if current {
            return;
        }
        if matches!(*status, LeaseStatus::Active | LeaseStatus::Releasing) {
            *status = LeaseStatus::Invalidated;
        }
    }

    fn assert_active(&self) -> Result<(), PiClientError> {
        self.refresh_status();
        if self.client.is_disposed() {
            return Err(PiClientError {
                message: "PiClient is disposed".into(),
            });
        }
        if !self.client.is_connected() {
            return Err(PiClientError {
                message: "client is disconnected".into(),
            });
        }
        if !self.is_active() {
            return Err(PiClientError {
                message: format!("Session {} is detached", self.id),
            });
        }
        Ok(())
    }

    async fn release(&self, relinquish_on_failure: bool) -> Result<(), PiClientError> {
        let (completion, owner) = loop {
            self.refresh_status();
            let current = *self.status.lock().unwrap();
            match current {
                LeaseStatus::Released | LeaseStatus::Invalidated => return Ok(()),
                LeaseStatus::Releasing => {
                    if let Some(completion) = self.release_completion.lock().unwrap().clone() {
                        break (completion, false);
                    }
                    tokio::task::yield_now().await;
                }
                LeaseStatus::Active => {
                    self.assert_active()?;
                    let mut status = self.status.lock().unwrap();
                    if !matches!(*status, LeaseStatus::Active) {
                        continue;
                    }
                    let completion = Arc::new(ReleaseCompletion::new());
                    *status = LeaseStatus::Releasing;
                    *self.release_completion.lock().unwrap() = Some(completion.clone());
                    break (completion, true);
                }
            }
        };

        if !owner {
            return completion.wait().await;
        }
        let result = self
            .client
            .release_lease_once(&self.token, relinquish_on_failure)
            .await;
        {
            let mut status = self.status.lock().unwrap();
            match &result {
                Ok(()) => *status = LeaseStatus::Released,
                Err(_) if relinquish_on_failure => *status = LeaseStatus::Released,
                Err(_) => *status = LeaseStatus::Active,
            }
        }
        completion.complete(result.clone());
        result
    }

    fn clear_subscriptions(&self) {
        let subscriptions = std::mem::take(&mut *self.subscriptions.lock().unwrap());
        for subscription in subscriptions {
            subscription.unsubscribe();
        }
    }
}

fn handle_is_active(
    client: &PiClient,
    token: &SessionLeaseToken,
    status: &Arc<Mutex<LeaseStatus>>,
) -> bool {
    let current = client.lease_generation_is_current(token);
    let active = matches!(&*status.lock().unwrap(), LeaseStatus::Active);
    current && active && client.is_connected() && client.is_session_attached(&token.session_id)
}

impl PiClient {
    /// Create a session and return a handle for the already-attached result.
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
        let token = self.reserve_session_lease(&session.id, options.mode)?;
        self.note_session_snapshot(session);
        let session_id = token.session_id.clone();
        let operation = self.session_operation(&session_id);
        let result = async {
            let _guard = operation.lock().await;
            if !self.is_session_attached(&session_id) {
                let previous = self.forget_session_snapshot(&session_id);
                let attach = self
                    .request(Command::Attach {
                        session_id: session_id.clone(),
                    })
                    .await;
                match attach {
                    Ok(CommandResult::Attach { session }) if session.id == session_id => {
                        self.note_session_snapshot(session);
                    }
                    Ok(CommandResult::Attach { session }) => {
                        if let Some(previous) = previous {
                            self.restore_session_snapshot(previous);
                        }
                        return Err(PiClientError {
                            message: format!("attach returned session {}", session.id),
                        });
                    }
                    Ok(_) => {
                        if let Some(previous) = previous {
                            self.restore_session_snapshot(previous);
                        }
                        return Err(PiClientError {
                            message: "unexpected command result for attach".into(),
                        });
                    }
                    Err(error) => {
                        if let Some(previous) = previous {
                            self.restore_session_snapshot(previous);
                        }
                        return Err(error);
                    }
                }
            }
            Ok(SessionHandle::new(self.clone(), token.clone()))
        }
        .await;
        if result.is_err() {
            self.release_session_lease(&token);
        }
        result
    }

    /// Acquire a shared lease for an existing session.
    pub async fn attach_session(&self, session_id: &str) -> Result<SessionHandle, PiClientError> {
        self.acquire_session(session_id, AcquireSessionOptions::default())
            .await
    }

    /// Acquire a shared or exclusive lease, reconciling failed disposal before
    /// issuing a new attach.
    pub async fn acquire_session(
        &self,
        session_id: &str,
        options: AcquireSessionOptions,
    ) -> Result<SessionHandle, PiClientError> {
        if self.is_disposed() {
            return Err(PiClientError {
                message: "PiClient is disposed".into(),
            });
        }
        let token = self.reserve_session_lease(session_id, options.mode)?;
        let operation = self.session_operation(session_id);
        let result = async {
            let _guard = operation.lock().await;
            let reconciled = self.reconcile_cleanup(session_id).await?;
            if reconciled || !self.is_session_attached(session_id) {
                let previous = self.forget_session_snapshot(session_id);
                let attach = self
                    .request(Command::Attach {
                        session_id: session_id.to_string(),
                    })
                    .await;
                match attach {
                    Ok(CommandResult::Attach { session }) if session.id == session_id => {
                        self.note_session_snapshot(session);
                    }
                    Ok(CommandResult::Attach { session }) => {
                        if let Some(previous) = previous {
                            self.restore_session_snapshot(previous);
                        }
                        return Err(PiClientError {
                            message: format!("attach returned session {}", session.id),
                        });
                    }
                    Ok(_) => {
                        if let Some(previous) = previous {
                            self.restore_session_snapshot(previous);
                        }
                        return Err(PiClientError {
                            message: "unexpected command result for attach".into(),
                        });
                    }
                    Err(error) => {
                        if let Some(previous) = previous {
                            self.restore_session_snapshot(previous);
                        }
                        return Err(error);
                    }
                }
            }
            Ok(SessionHandle::new(self.clone(), token.clone()))
        }
        .await;
        if result.is_err() {
            self.release_session_lease(&token);
        }
        result
    }

    async fn reconcile_cleanup(&self, session_id: &str) -> Result<bool, PiClientError> {
        if !self.take_cleanup_required(session_id) {
            return Ok(false);
        }
        let result = self
            .request(Command::Detach {
                session_id: session_id.to_string(),
            })
            .await
            .and_then(|result| match result {
                CommandResult::Detach {
                    session_id: detached,
                } if detached == session_id => Ok(()),
                CommandResult::Detach {
                    session_id: detached,
                } => Err(PiClientError {
                    message: format!("detach returned wrong session {detached}"),
                }),
                _ => Err(PiClientError {
                    message: "unexpected command result for detach".into(),
                }),
            });
        if let Err(error) = result {
            self.mark_cleanup_required(session_id);
            return Err(error);
        }
        Ok(true)
    }
}
