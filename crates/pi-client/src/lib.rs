//! Pi protocol client — port of packages/client.
//!
//! The client owns protocol framing, request correlation, connection
//! lifecycle, session lease bookkeeping, and snapshot reduction. Transports
//! are deliberately byte-oriented so deterministic in-memory factories can
//! exercise the same behavior as the Unix socket adapter.

pub mod session_handle;
pub mod transport;

pub use session_handle::{AcquireSessionOptions, SessionHandle, SessionLeaseMode};
pub use transport::{ByteTransport, TransportFactory, TransportHandlers, UnixTransportFactory};

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_protocol::{
    ClientMessage, Command, CommandResult, FrameDecoderOptions, ServerEvent, ServerMessage,
    ServerMessageDecoder, ServerSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiClientError {
    pub message: String,
}

impl std::fmt::Display for PiClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiClientError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone)]
pub struct ConnectionStateChange {
    pub state: ClientConnectionState,
    pub error: Option<PiClientError>,
}

pub type EventListener = Arc<dyn Fn(&ServerEvent) + Send + Sync>;
pub type ConnectionStateListener = Arc<dyn Fn(&ConnectionStateChange) + Send + Sync>;
pub type ConnectionStateUnsubscribe = Box<dyn Fn() + Send + Sync>;

/// Deterministic reconnect retry policy. reconnect() remains a single
/// upstream-compatible connection attempt; this explicit helper adds bounded
/// exponential backoff for callers that want retry behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectBackoff {
    pub max_attempts: usize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        }
    }
}

impl ReconnectBackoff {
    fn delay_before_retry(self, retry_index: usize) -> Duration {
        let shift = retry_index.min(31) as u32;
        let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.initial_delay
            .saturating_mul(multiplier)
            .min(self.max_delay)
    }
}

struct Pending {
    command: Command,
    resolve: tokio::sync::oneshot::Sender<Result<CommandResult, PiClientError>>,
}

enum PendingEntry {
    Active(Pending),
    TimedOut,
}

type HandshakeSender = tokio::sync::oneshot::Sender<Result<ServerSnapshot, PiClientError>>;
type SnapshotListener = Arc<dyn Fn(&ServerSnapshot) + Send + Sync>;
type SessionSnapshotListener = Arc<dyn Fn(&pi_protocol::SessionSnapshot) + Send + Sync>;
type SessionEventListener = Arc<dyn Fn(&ServerEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub(crate) struct SessionLeaseToken {
    pub(crate) id: u64,
    pub(crate) session_id: String,
    pub(crate) generation: u64,
}

#[derive(Default)]
struct LeaseRegistry {
    active: HashMap<u64, String>,
    counts: HashMap<String, usize>,
    exclusive: HashMap<String, u64>,
    generations: HashMap<String, u64>,
    cleanup_required: HashSet<String>,
}

#[derive(Clone)]
pub struct PiClient {
    transport_factory: Arc<dyn transport::TransportFactory>,
    connection: Arc<Mutex<Option<Arc<dyn transport::ByteTransport>>>>,
    pending: Arc<Mutex<HashMap<String, PendingEntry>>>,
    listeners: Arc<Mutex<Vec<EventListener>>>,
    connection_state: Arc<Mutex<ClientConnectionState>>,
    connection_state_listeners: Arc<Mutex<Vec<Option<ConnectionStateListener>>>>,
    snapshot: Arc<Mutex<Option<ServerSnapshot>>>,
    snapshot_listeners: Arc<Mutex<Vec<Option<SnapshotListener>>>>,
    session_snapshots: Arc<Mutex<HashMap<String, pi_protocol::SessionSnapshot>>>,
    attached_sessions: Arc<Mutex<HashSet<String>>>,
    session_snapshot_listeners: Arc<Mutex<HashMap<String, Vec<Option<SessionSnapshotListener>>>>>,
    session_event_listeners: Arc<Mutex<HashMap<String, Vec<Option<SessionEventListener>>>>>,
    handshake: Arc<Mutex<Option<HandshakeSender>>>,
    next_request_id: Arc<AtomicU64>,
    next_lease_id: Arc<AtomicU64>,
    connection_epoch: Arc<AtomicU64>,
    disposed: Arc<AtomicBool>,
    lease_registry: Arc<Mutex<LeaseRegistry>>,
    session_operations: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    handshake_timeout: Duration,
    request_timeout: Duration,
    max_frame_length: usize,
}

impl PiClient {
    pub fn new(transport_factory: Arc<dyn transport::TransportFactory>) -> Self {
        Self::with_transport_factory(
            transport_factory,
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
    }

    pub fn with_transport_factory(
        transport_factory: Arc<dyn transport::TransportFactory>,
        handshake_timeout: Duration,
        request_timeout: Duration,
    ) -> Self {
        Self {
            transport_factory,
            connection: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            connection_state: Arc::new(Mutex::new(ClientConnectionState::Disconnected)),
            connection_state_listeners: Arc::new(Mutex::new(Vec::new())),
            snapshot: Arc::new(Mutex::new(None)),
            snapshot_listeners: Arc::new(Mutex::new(Vec::new())),
            session_snapshots: Arc::new(Mutex::new(HashMap::new())),
            attached_sessions: Arc::new(Mutex::new(HashSet::new())),
            session_snapshot_listeners: Arc::new(Mutex::new(HashMap::new())),
            session_event_listeners: Arc::new(Mutex::new(HashMap::new())),
            handshake: Arc::new(Mutex::new(None)),
            next_request_id: Arc::new(AtomicU64::new(1)),
            next_lease_id: Arc::new(AtomicU64::new(1)),
            connection_epoch: Arc::new(AtomicU64::new(0)),
            disposed: Arc::new(AtomicBool::new(false)),
            lease_registry: Arc::new(Mutex::new(LeaseRegistry::default())),
            session_operations: Arc::new(Mutex::new(HashMap::new())),
            handshake_timeout,
            request_timeout,
            max_frame_length: pi_protocol::DEFAULT_MAX_FRAME_LENGTH,
        }
    }

    pub fn with_transport_factory_and_frame_limit(
        transport_factory: Arc<dyn transport::TransportFactory>,
        handshake_timeout: Duration,
        request_timeout: Duration,
        max_frame_length: usize,
    ) -> Result<Self, PiClientError> {
        if max_frame_length == 0 || max_frame_length > u32::MAX as usize {
            return Err(PiClientError {
                message: format!(
                    "max_frame_length must be an integer between 1 and {}",
                    u32::MAX
                ),
            });
        }
        let mut client =
            Self::with_transport_factory(transport_factory, handshake_timeout, request_timeout);
        client.max_frame_length = max_frame_length;
        Ok(client)
    }

    pub async fn connect(socket_path: &str) -> Result<Self, PiClientError> {
        Self::connect_with_timeouts(socket_path, Duration::from_secs(5), Duration::from_secs(30))
            .await
    }

    pub async fn connect_with_timeouts(
        socket_path: &str,
        handshake_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, PiClientError> {
        let factory = Arc::new(UnixTransportFactory::new(socket_path)?);
        Self::connect_with_transport_factory(factory, handshake_timeout, request_timeout).await
    }

    pub async fn connect_with_transport_factory(
        transport_factory: Arc<dyn transport::TransportFactory>,
        handshake_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, PiClientError> {
        let client =
            Self::with_transport_factory(transport_factory, handshake_timeout, request_timeout);
        if let Err(error) = client.reconnect().await {
            client.dispose().await?;
            return Err(error);
        }
        Ok(client)
    }

    pub fn connection_state(&self) -> ClientConnectionState {
        *self.connection_state.lock().unwrap()
    }

    pub fn is_connected(&self) -> bool {
        self.connection_state() == ClientConnectionState::Connected
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    pub fn subscribe_connection_state(
        &self,
        listener: impl Fn(&ConnectionStateChange) + Send + Sync + 'static,
    ) -> ConnectionStateUnsubscribe {
        let listener: ConnectionStateListener = Arc::new(listener);
        let mut listeners = self.connection_state_listeners.lock().unwrap();
        listeners.push(Some(listener));
        let index = listeners.len() - 1;
        let listeners = self.connection_state_listeners.clone();
        Box::new(move || {
            if let Some(slot) = listeners.lock().unwrap().get_mut(index) {
                *slot = None;
            }
        })
    }

    pub fn subscribe(&self, listener: EventListener) {
        self.listeners.lock().unwrap().push(listener);
    }

    pub fn subscribe_snapshot(
        &self,
        listener: impl Fn(&ServerSnapshot) + Send + Sync + 'static,
    ) -> ConnectionStateUnsubscribe {
        let mut listeners = self.snapshot_listeners.lock().unwrap();
        listeners.push(Some(Arc::new(listener)));
        let index = listeners.len() - 1;
        let listeners = self.snapshot_listeners.clone();
        Box::new(move || {
            if let Some(slot) = listeners.lock().unwrap().get_mut(index) {
                *slot = None;
            }
        })
    }

    pub fn snapshot(&self) -> Option<ServerSnapshot> {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn session_snapshot(&self, session_id: &str) -> Option<pi_protocol::SessionSnapshot> {
        self.session_snapshots
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
    }

    pub async fn reconnect(&self) -> Result<ServerSnapshot, PiClientError> {
        if self.is_disposed() {
            return Err(error("PiClient is disposed"));
        }
        {
            let mut state = self.connection_state.lock().unwrap();
            if *state != ClientConnectionState::Disconnected {
                return Err(error(format!("PiClient is already {}", state_name(*state))));
            }
            *state = ClientConnectionState::Connecting;
        }
        self.notify_connection_state(ConnectionStateChange {
            state: ClientConnectionState::Connecting,
            error: None,
        });
        self.reset_connection_state();

        let epoch = self.connection_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();
        *self.handshake.lock().unwrap() = Some(handshake_tx);
        let decoder = Arc::new(Mutex::new(
            ServerMessageDecoder::new(&FrameDecoderOptions {
                max_frame_length: Some(self.max_frame_length),
            })
            .map_err(|protocol_error| PiClientError {
                message: protocol_error.to_string(),
            })?,
        ));

        let data_client = self.clone();
        let data_decoder = decoder.clone();
        let close_client = self.clone();
        let close_decoder = decoder.clone();
        let error_client = self.clone();
        let handlers = TransportHandlers::new(
            move |chunk| data_client.handle_data(epoch, data_decoder.clone(), chunk),
            move || close_client.handle_close(epoch, close_decoder.clone()),
            move |transport_error| error_client.connection_lost(epoch, transport_error),
        );

        let connect_attempt = async {
            let transport = match self.transport_factory.connect(handlers).await {
                Ok(transport) => transport,
                Err(connect_error) => {
                    self.connection_lost(epoch, connect_error.clone());
                    return Err(connect_error);
                }
            };
            if self.connection_epoch.load(Ordering::SeqCst) != epoch
                || self.connection_state() != ClientConnectionState::Connecting
            {
                transport.close();
                return Err(error("connection attempt is no longer current"));
            }
            *self.connection.lock().unwrap() = Some(transport.clone());
            let frame = pi_protocol::encode_client_message(
                &ClientMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION,
                },
                &FrameDecoderOptions {
                    max_frame_length: Some(self.max_frame_length),
                },
            )
            .map_err(|protocol_error| PiClientError {
                message: format!("encode: {protocol_error}"),
            })?;
            if let Err(send_error) = transport.send(frame).await {
                self.connection_lost(epoch, send_error.clone());
                return Err(send_error);
            }
            match handshake_rx.await {
                Ok(result) => result,
                Err(_) => Err(error("connection closed during handshake")),
            }
        };

        match tokio::time::timeout(self.handshake_timeout, connect_attempt).await {
            Ok(result) => result,
            Err(_) => {
                let timeout_error = error(format!(
                    "handshake timed out after {}ms",
                    self.handshake_timeout.as_millis()
                ));
                self.connection_lost(epoch, timeout_error.clone());
                Err(timeout_error)
            }
        }
    }

    pub async fn reconnect_with_backoff(
        &self,
        policy: ReconnectBackoff,
    ) -> Result<ServerSnapshot, PiClientError> {
        if policy.max_attempts == 0 {
            return Err(error("reconnect max_attempts must be positive"));
        }
        let mut last_error = None;
        for attempt in 0..policy.max_attempts {
            if attempt > 0 {
                tokio::time::sleep(policy.delay_before_retry(attempt - 1)).await;
            }
            match self.reconnect().await {
                Ok(snapshot) => return Ok(snapshot),
                Err(reconnect_error) => {
                    if self.is_disposed() {
                        return Err(reconnect_error);
                    }
                    last_error = Some(reconnect_error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| error("reconnect failed")))
    }

    pub async fn request(&self, command: Command) -> Result<CommandResult, PiClientError> {
        if self.is_disposed() {
            return Err(error("PiClient is disposed"));
        }
        if !self.is_connected() {
            return Err(error("client is disconnected"));
        }
        let epoch = self.connection_epoch.load(Ordering::SeqCst);
        let transport = self
            .connection
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| error("client is disconnected"))?;
        let id = format!("r{}", self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let frame = pi_protocol::encode_client_message(
            &ClientMessage::Request {
                id: id.clone(),
                request: command.clone(),
            },
            &FrameDecoderOptions {
                max_frame_length: Some(self.max_frame_length),
            },
        )
        .map_err(|protocol_error| PiClientError {
            message: format!("encode: {protocol_error}"),
        })?;
        let (resolve, receive) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(
            id.clone(),
            PendingEntry::Active(Pending { command, resolve }),
        );
        if let Err(send_error) = transport.send(frame).await {
            self.pending.lock().unwrap().remove(&id);
            self.connection_lost(epoch, send_error.clone());
            return Err(send_error);
        }

        match tokio::time::timeout(self.request_timeout, receive).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(error("connection closed before response")),
            Err(_) => {
                let mut pending = self.pending.lock().unwrap();
                if matches!(pending.get(&id), Some(PendingEntry::Active(_))) {
                    pending.insert(id, PendingEntry::TimedOut);
                }
                Err(error(format!(
                    "request timed out after {}ms",
                    self.request_timeout.as_millis()
                )))
            }
        }
    }

    pub async fn close(&self) -> Result<(), PiClientError> {
        if self.is_connected() || self.connection_state() == ClientConnectionState::Connecting {
            self.connection_lost(
                self.connection_epoch.load(Ordering::SeqCst),
                error("Client disconnected"),
            );
        }
        Ok(())
    }

    pub async fn dispose(&self) -> Result<(), PiClientError> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let disposed_error = error("PiClient is disposed");
        self.reject_pending(disposed_error.clone());
        if self.is_connected() || self.connection_state() == ClientConnectionState::Connecting {
            self.connection_lost(self.connection_epoch.load(Ordering::SeqCst), disposed_error);
        } else {
            self.reset_connection_state();
            self.invalidate_all_leases();
        }
        self.listeners.lock().unwrap().clear();
        self.connection_state_listeners.lock().unwrap().clear();
        self.snapshot_listeners.lock().unwrap().clear();
        self.session_snapshot_listeners.lock().unwrap().clear();
        self.session_event_listeners.lock().unwrap().clear();
        Ok(())
    }

    fn handle_data(&self, epoch: u64, decoder: Arc<Mutex<ServerMessageDecoder>>, chunk: Vec<u8>) {
        if self.connection_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        let messages = match decoder.lock().unwrap().push(&chunk) {
            Ok(messages) => messages,
            Err(protocol_error) => {
                self.connection_lost(
                    epoch,
                    PiClientError {
                        message: protocol_error.to_string(),
                    },
                );
                return;
            }
        };
        for message in messages {
            if self.connection_epoch.load(Ordering::SeqCst) != epoch
                || self.connection_state() == ClientConnectionState::Disconnected
            {
                return;
            }
            self.handle_message(epoch, message);
        }
    }

    fn handle_close(&self, epoch: u64, decoder: Arc<Mutex<ServerMessageDecoder>>) {
        let connection_error = match decoder.lock().unwrap().end() {
            Ok(()) => error("Byte transport closed"),
            Err(protocol_error) => PiClientError {
                message: protocol_error.to_string(),
            },
        };
        self.connection_lost(epoch, connection_error);
    }

    fn handle_message(&self, epoch: u64, message: ServerMessage) {
        if self.connection_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        match message {
            ServerMessage::Hello { snapshot, .. } => {
                if self.connection_state() != ClientConnectionState::Connecting {
                    self.connection_lost(epoch, error("unexpected handshake message"));
                    return;
                }
                self.apply_server_snapshot(snapshot.clone());
                if self.connection_state() != ClientConnectionState::Connecting {
                    return;
                }
                self.set_connection_state(ClientConnectionState::Connected, None);
                if self.connection_epoch.load(Ordering::SeqCst) != epoch
                    || self.connection_state() != ClientConnectionState::Connected
                {
                    return;
                }
                if let Some(sender) = self.handshake.lock().unwrap().take() {
                    let _ = sender.send(Ok(snapshot));
                }
            }
            ServerMessage::HelloError {
                error: protocol_error,
            } => {
                self.connection_lost(
                    epoch,
                    PiClientError {
                        message: format!("{:?}: {}", protocol_error.code, protocol_error.message),
                    },
                );
            }
            ServerMessage::Response {
                id,
                ok,
                result,
                error: protocol_error,
            } => {
                let pending = self.pending.lock().unwrap().remove(&id);
                let Some(pending) = pending else {
                    self.connection_lost(
                        epoch,
                        error(format!("response has no matching request: {id}")),
                    );
                    return;
                };
                let PendingEntry::Active(Pending { command, resolve }) = pending else {
                    return;
                };
                if !ok {
                    let protocol_error = protocol_error.unwrap_or(pi_protocol::ProtocolError {
                        code: pi_protocol::ProtocolErrorCode::InternalError,
                        message: "unknown error".into(),
                        details: None,
                    });
                    let _ = resolve.send(Err(PiClientError {
                        message: format!("{:?}: {}", protocol_error.code, protocol_error.message),
                    }));
                    return;
                }
                let Some(result) = result else {
                    let _ = resolve.send(Err(error("response with no result")));
                    return;
                };
                if !command_matches(&command, &result) {
                    let mismatch = error(format!(
                        "response command {} does not match {}",
                        result_name(&result),
                        command_name(&command)
                    ));
                    let _ = resolve.send(Err(mismatch.clone()));
                    self.connection_lost(epoch, mismatch);
                    return;
                }
                self.apply_command_result(&result);
                let _ = resolve.send(Ok(result));
            }
            ServerMessage::Event { event } => {
                if self.connection_state() != ClientConnectionState::Connected {
                    self.connection_lost(epoch, error("received event before handshake"));
                    return;
                }
                self.apply_event(&event);
                self.notify_event_listeners(&event);
            }
        }
    }

    fn connection_lost(&self, epoch: u64, connection_error: PiClientError) {
        if self.connection_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        let changed = {
            let mut state = self.connection_state.lock().unwrap();
            if *state == ClientConnectionState::Disconnected {
                false
            } else {
                *state = ClientConnectionState::Disconnected;
                true
            }
        };
        if !changed {
            return;
        }
        let connection = self.connection.lock().unwrap().take();
        let handshake = self.handshake.lock().unwrap().take();
        self.reject_pending(connection_error.clone());
        self.reset_connection_state();
        self.invalidate_all_leases();
        if let Some(sender) = handshake {
            let _ = sender.send(Err(connection_error.clone()));
        }
        self.notify_connection_state(ConnectionStateChange {
            state: ClientConnectionState::Disconnected,
            error: Some(connection_error),
        });
        if let Some(connection) = connection {
            connection.close();
        }
    }

    fn reject_pending(&self, connection_error: PiClientError) {
        let entries = std::mem::take(&mut *self.pending.lock().unwrap());
        for entry in entries.into_values() {
            if let PendingEntry::Active(Pending { resolve, .. }) = entry {
                let _ = resolve.send(Err(connection_error.clone()));
            }
        }
    }

    fn reset_connection_state(&self) {
        *self.snapshot.lock().unwrap() = None;
        self.session_snapshots.lock().unwrap().clear();
        self.attached_sessions.lock().unwrap().clear();
    }

    fn set_connection_state(
        &self,
        state: ClientConnectionState,
        state_error: Option<PiClientError>,
    ) {
        let changed = {
            let mut current = self.connection_state.lock().unwrap();
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        };
        if changed {
            self.notify_connection_state(ConnectionStateChange {
                state,
                error: state_error,
            });
        }
    }

    fn notify_connection_state(&self, change: ConnectionStateChange) {
        let listeners = self.connection_state_listeners.lock().unwrap().clone();
        for listener in listeners.into_iter().flatten() {
            let _ = catch_unwind(AssertUnwindSafe(|| listener(&change)));
        }
    }

    fn apply_server_snapshot(&self, snapshot: ServerSnapshot) {
        let should_apply = {
            let mut current = self.snapshot.lock().unwrap();
            if current
                .as_ref()
                .is_some_and(|previous| snapshot.revision < previous.revision)
            {
                false
            } else {
                *current = Some(snapshot.clone());
                true
            }
        };
        if !should_apply {
            return;
        }
        let listeners = self.snapshot_listeners.lock().unwrap().clone();
        for listener in listeners.into_iter().flatten() {
            let _ = catch_unwind(AssertUnwindSafe(|| listener(&snapshot)));
        }
    }

    fn apply_command_result(&self, result: &CommandResult) {
        match result {
            CommandResult::List { .. } => {}
            CommandResult::Detach { session_id } => {
                let previous = self.session_snapshot(session_id);
                if let Some(mut snapshot) = previous {
                    snapshot.attached = false;
                    self.apply_session_snapshot(snapshot, true);
                } else {
                    self.attached_sessions.lock().unwrap().remove(session_id);
                }
            }
            CommandResult::Create { session }
            | CommandResult::Attach { session }
            | CommandResult::Prompt { session }
            | CommandResult::Steer { session }
            | CommandResult::Abort { session }
            | CommandResult::SetModel { session }
            | CommandResult::SetThinking { session } => {
                self.apply_session_snapshot(session.clone(), false);
            }
        }
    }

    fn apply_event(&self, event: &ServerEvent) {
        match event {
            ServerEvent::ServerSnapshot { snapshot } => {
                self.apply_server_snapshot(snapshot.clone())
            }
            ServerEvent::SessionSnapshot { snapshot } => {
                self.apply_session_snapshot(snapshot.clone(), false)
            }
            ServerEvent::SessionRemoved { session_id } => {
                self.invalidate_session_leases(session_id);
                self.session_snapshots.lock().unwrap().remove(session_id);
                self.attached_sessions.lock().unwrap().remove(session_id);
            }
            ServerEvent::SessionProgress { .. } => {}
        }
    }

    fn apply_session_snapshot(&self, snapshot: pi_protocol::SessionSnapshot, force: bool) {
        let should_apply = {
            let mut snapshots = self.session_snapshots.lock().unwrap();
            let should_apply = force
                || snapshots
                    .get(&snapshot.id)
                    .is_none_or(|previous| snapshot.revision >= previous.revision);
            if should_apply {
                snapshots.insert(snapshot.id.clone(), snapshot.clone());
            }
            should_apply
        };
        if !should_apply {
            return;
        }
        if snapshot.attached || (!force && self.has_active_session_lease(&snapshot.id)) {
            self.attached_sessions
                .lock()
                .unwrap()
                .insert(snapshot.id.clone());
        } else {
            self.attached_sessions.lock().unwrap().remove(&snapshot.id);
        }
        let listeners = self
            .session_snapshot_listeners
            .lock()
            .unwrap()
            .get(&snapshot.id)
            .cloned()
            .unwrap_or_default();
        for listener in listeners.into_iter().flatten() {
            let _ = catch_unwind(AssertUnwindSafe(|| listener(&snapshot)));
        }
    }

    fn notify_event_listeners(&self, event: &ServerEvent) {
        let listeners = self.listeners.lock().unwrap().clone();
        for listener in listeners {
            let _ = catch_unwind(AssertUnwindSafe(|| listener(event)));
        }
        let session_id = match event {
            ServerEvent::SessionSnapshot { snapshot } => Some(snapshot.id.as_str()),
            ServerEvent::SessionProgress { session_id, .. }
            | ServerEvent::SessionRemoved { session_id } => Some(session_id.as_str()),
            ServerEvent::ServerSnapshot { .. } => None,
        };
        if let Some(session_id) = session_id {
            let listeners = self
                .session_event_listeners
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_default();
            for listener in listeners.into_iter().flatten() {
                let _ = catch_unwind(AssertUnwindSafe(|| listener(event)));
            }
        }
    }

    pub(crate) fn note_session_snapshot(&self, snapshot: pi_protocol::SessionSnapshot) {
        self.apply_session_snapshot(snapshot, false);
    }

    fn has_active_session_lease(&self, session_id: &str) -> bool {
        self.lease_registry
            .lock()
            .unwrap()
            .counts
            .get(session_id)
            .copied()
            .unwrap_or(0)
            > 0
    }

    pub(crate) fn is_session_attached(&self, session_id: &str) -> bool {
        self.attached_sessions.lock().unwrap().contains(session_id)
    }

    pub(crate) fn forget_session_snapshot(
        &self,
        session_id: &str,
    ) -> Option<pi_protocol::SessionSnapshot> {
        self.attached_sessions.lock().unwrap().remove(session_id);
        self.session_snapshots.lock().unwrap().remove(session_id)
    }

    pub(crate) fn restore_session_snapshot(&self, snapshot: pi_protocol::SessionSnapshot) {
        let mut snapshots = self.session_snapshots.lock().unwrap();
        if snapshots.contains_key(&snapshot.id) {
            return;
        }
        let attached = snapshot.attached;
        let id = snapshot.id.clone();
        snapshots.insert(id.clone(), snapshot);
        drop(snapshots);
        if attached {
            self.attached_sessions.lock().unwrap().insert(id);
        }
    }

    pub(crate) fn subscribe_session_snapshots(
        &self,
        session_id: &str,
        listener: impl Fn(&pi_protocol::SessionSnapshot) + Send + Sync + 'static,
    ) -> ConnectionStateUnsubscribe {
        let mut listeners = self.session_snapshot_listeners.lock().unwrap();
        let entries = listeners.entry(session_id.to_string()).or_default();
        entries.push(Some(Arc::new(listener)));
        let index = entries.len() - 1;
        let listeners = self.session_snapshot_listeners.clone();
        let session_id = session_id.to_string();
        Box::new(move || {
            if let Some(entries) = listeners.lock().unwrap().get_mut(&session_id) {
                if let Some(slot) = entries.get_mut(index) {
                    *slot = None;
                }
            }
        })
    }

    pub(crate) fn subscribe_session_events(
        &self,
        session_id: &str,
        listener: impl Fn(&ServerEvent) + Send + Sync + 'static,
    ) -> ConnectionStateUnsubscribe {
        let mut listeners = self.session_event_listeners.lock().unwrap();
        let entries = listeners.entry(session_id.to_string()).or_default();
        entries.push(Some(Arc::new(listener)));
        let index = entries.len() - 1;
        let listeners = self.session_event_listeners.clone();
        let session_id = session_id.to_string();
        Box::new(move || {
            if let Some(entries) = listeners.lock().unwrap().get_mut(&session_id) {
                if let Some(slot) = entries.get_mut(index) {
                    *slot = None;
                }
            }
        })
    }

    pub(crate) fn session_operation(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.session_operations
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub(crate) fn reserve_session_lease(
        &self,
        session_id: &str,
        mode: SessionLeaseMode,
    ) -> Result<SessionLeaseToken, PiClientError> {
        let mut registry = self.lease_registry.lock().unwrap();
        let count = registry.counts.get(session_id).copied().unwrap_or(0);
        if mode == SessionLeaseMode::Exclusive && count > 0 {
            return Err(error(format!(
                "Session {session_id} already has an active lease"
            )));
        }
        if mode == SessionLeaseMode::Shared && registry.exclusive.contains_key(session_id) {
            return Err(error(format!(
                "Session {session_id} has an exclusive lease"
            )));
        }
        let id = self.next_lease_id.fetch_add(1, Ordering::Relaxed);
        let generation = registry.generations.get(session_id).copied().unwrap_or(0);
        registry.active.insert(id, session_id.to_string());
        registry.counts.insert(session_id.to_string(), count + 1);
        if mode == SessionLeaseMode::Exclusive {
            registry.exclusive.insert(session_id.to_string(), id);
        }
        Ok(SessionLeaseToken {
            id,
            session_id: session_id.to_string(),
            generation,
        })
    }

    pub(crate) fn release_session_lease(&self, token: &SessionLeaseToken) {
        let mut registry = self.lease_registry.lock().unwrap();
        let Some(session_id) = registry.active.remove(&token.id) else {
            return;
        };
        let count = registry.counts.get(&session_id).copied().unwrap_or(1);
        if count <= 1 {
            registry.counts.remove(&session_id);
        } else {
            registry.counts.insert(session_id.clone(), count - 1);
        }
        if registry.exclusive.get(&session_id) == Some(&token.id) {
            registry.exclusive.remove(&session_id);
        }
    }

    pub(crate) fn lease_is_reserved(&self, token: &SessionLeaseToken) -> bool {
        let registry = self.lease_registry.lock().unwrap();
        registry.active.get(&token.id) == Some(&token.session_id)
            && registry
                .generations
                .get(&token.session_id)
                .copied()
                .unwrap_or(0)
                == token.generation
    }

    pub(crate) fn lease_generation_is_current(&self, token: &SessionLeaseToken) -> bool {
        self.lease_registry
            .lock()
            .unwrap()
            .generations
            .get(&token.session_id)
            .copied()
            .unwrap_or(0)
            == token.generation
    }

    pub(crate) fn mark_cleanup_required(&self, session_id: &str) {
        self.lease_registry
            .lock()
            .unwrap()
            .cleanup_required
            .insert(session_id.to_string());
    }

    pub(crate) fn take_cleanup_required(&self, session_id: &str) -> bool {
        self.lease_registry
            .lock()
            .unwrap()
            .cleanup_required
            .remove(session_id)
    }

    pub(crate) fn invalidate_session_leases(&self, session_id: &str) {
        let mut registry = self.lease_registry.lock().unwrap();
        registry.active.retain(|_, id| id != session_id);
        registry.counts.remove(session_id);
        registry.exclusive.remove(session_id);
        registry.cleanup_required.remove(session_id);
        let generation = registry
            .generations
            .entry(session_id.to_string())
            .or_default();
        *generation = generation.wrapping_add(1);
    }

    fn invalidate_all_leases(&self) {
        let mut registry = self.lease_registry.lock().unwrap();
        let mut session_ids: HashSet<String> = registry.active.values().cloned().collect();
        session_ids.extend(registry.counts.keys().cloned());
        session_ids.extend(registry.exclusive.keys().cloned());
        registry.active.clear();
        registry.counts.clear();
        registry.exclusive.clear();
        registry.cleanup_required.clear();
        for session_id in session_ids {
            let generation = registry.generations.entry(session_id).or_default();
            *generation = generation.wrapping_add(1);
        }
    }

    pub(crate) async fn release_lease_once(
        &self,
        token: &SessionLeaseToken,
        relinquish_on_failure: bool,
    ) -> Result<(), PiClientError> {
        if !self.lease_is_reserved(token) {
            return Ok(());
        }
        let operation = self.session_operation(&token.session_id);
        let _guard = operation.lock().await;
        if !self.lease_is_reserved(token) {
            return Ok(());
        }
        let count = self
            .lease_registry
            .lock()
            .unwrap()
            .counts
            .get(&token.session_id)
            .copied()
            .unwrap_or(0);
        if count > 1 {
            self.release_session_lease(token);
            return Ok(());
        }
        let result = self
            .request(Command::Detach {
                session_id: token.session_id.clone(),
            })
            .await
            .and_then(|result| match result {
                CommandResult::Detach { session_id } if session_id == token.session_id => Ok(()),
                CommandResult::Detach { session_id } => {
                    Err(error(format!("detach returned wrong session {session_id}")))
                }
                _ => Err(error("unexpected command result for detach")),
            });
        match result {
            Ok(()) => {
                self.release_session_lease(token);
                Ok(())
            }
            Err(_error)
                if !self.lease_is_reserved(token) || !self.lease_generation_is_current(token) =>
            {
                Ok(())
            }
            Err(release_error) => {
                if relinquish_on_failure {
                    self.release_session_lease(token);
                    self.mark_cleanup_required(&token.session_id);
                }
                Err(release_error)
            }
        }
    }
}

fn error(message: impl Into<String>) -> PiClientError {
    PiClientError {
        message: message.into(),
    }
}

fn state_name(state: ClientConnectionState) -> &'static str {
    match state {
        ClientConnectionState::Disconnected => "disconnected",
        ClientConnectionState::Connecting => "connecting",
        ClientConnectionState::Connected => "connected",
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::List => "list",
        Command::Create { .. } => "create",
        Command::Attach { .. } => "attach",
        Command::Detach { .. } => "detach",
        Command::Prompt { .. } => "prompt",
        Command::Steer { .. } => "steer",
        Command::Abort { .. } => "abort",
        Command::SetModel { .. } => "set_model",
        Command::SetThinking { .. } => "set_thinking",
    }
}

fn result_name(result: &CommandResult) -> &'static str {
    match result {
        CommandResult::List { .. } => "list",
        CommandResult::Create { .. } => "create",
        CommandResult::Attach { .. } => "attach",
        CommandResult::Detach { .. } => "detach",
        CommandResult::Prompt { .. } => "prompt",
        CommandResult::Steer { .. } => "steer",
        CommandResult::Abort { .. } => "abort",
        CommandResult::SetModel { .. } => "set_model",
        CommandResult::SetThinking { .. } => "set_thinking",
    }
}

fn command_matches(command: &Command, result: &CommandResult) -> bool {
    command_name(command) == result_name(result)
}
