//! Pi protocol client — port of `packages/client`.
//!
//! Connects to a `PiServer` over a Unix-domain socket, performs the hello
//! handshake, sends `Command`s with request/response correlation, and emits
//! `ServerEvent`s (server/session snapshots, progress) to subscribers.

pub mod session_handle;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_protocol::{
    ClientMessage, Command, CommandResult, ServerEvent, ServerMessage, ServerSnapshot,
};

/// Client error codes (upstream errors.ts `toError` mapping).
#[derive(Debug, Clone)]
pub struct PiClientError {
    pub message: String,
}

impl std::fmt::Display for PiClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for PiClientError {}

/// A pending request awaiting a response envelope.
struct Pending {
    resolve: tokio::sync::oneshot::Sender<Result<CommandResult, PiClientError>>,
}

pub type EventListener = Arc<dyn Fn(&ServerEvent) + Send + Sync>;

/// Connection lifecycle state exposed by the client (upstream
/// `ConnectionState`).
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

pub type ConnectionStateListener = Arc<dyn Fn(&ConnectionStateChange) + Send + Sync>;
pub type ConnectionStateUnsubscribe = Box<dyn Fn() + Send + Sync>;

type HandshakeWaiter =
    Arc<Mutex<Option<tokio::sync::oneshot::Sender<Result<ServerSnapshot, PiClientError>>>>>;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// PiClient: connects, handshakes, and issues protocol commands.
/// Clone is cheap: all mutable state is Arc-shared.
#[derive(Clone)]
pub struct PiClient {
    connection: Arc<Mutex<Option<transport::ClientConnection>>>,
    socket_path: Arc<String>,
    pending: Arc<Mutex<HashMap<String, Option<Pending>>>>,
    timed_out: Arc<Mutex<HashSet<String>>>,
    listeners: Arc<Mutex<Vec<EventListener>>>,
    connection_state: Arc<Mutex<ClientConnectionState>>,
    connection_state_listeners: Arc<Mutex<Vec<Option<ConnectionStateListener>>>>,
    snapshot: Arc<Mutex<Option<ServerSnapshot>>>,
    session_snapshots: Arc<Mutex<HashMap<String, pi_protocol::SessionSnapshot>>>,
    next_request_id: Arc<AtomicU64>,
    connection_epoch: Arc<AtomicU64>,
    disposed: Arc<AtomicBool>,
    handshake_timeout: Duration,
    request_timeout: Duration,
}

impl PiClient {
    pub async fn connect(socket_path: &str) -> Result<Self, PiClientError> {
        Self::connect_with_timeouts(
            socket_path,
            DEFAULT_HANDSHAKE_TIMEOUT,
            DEFAULT_REQUEST_TIMEOUT,
        )
        .await
    }

    /// Connect with explicit handshake/request bounds. The ordinary
    /// `connect` surface uses conservative defaults; this seam keeps timeout
    /// behavior deterministic for callers and conformance tests.
    pub async fn connect_with_timeouts(
        socket_path: &str,
        handshake_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, PiClientError> {
        let client = Self {
            connection: Arc::new(Mutex::new(None)),
            socket_path: Arc::new(socket_path.to_string()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            timed_out: Arc::new(Mutex::new(HashSet::new())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            connection_state: Arc::new(Mutex::new(ClientConnectionState::Disconnected)),
            connection_state_listeners: Arc::new(Mutex::new(Vec::new())),
            snapshot: Arc::new(Mutex::new(None)),
            session_snapshots: Arc::new(Mutex::new(HashMap::new())),
            next_request_id: Arc::new(AtomicU64::new(1)),
            connection_epoch: Arc::new(AtomicU64::new(0)),
            disposed: Arc::new(AtomicBool::new(false)),
            handshake_timeout,
            request_timeout,
        };
        client.reconnect().await?;
        Ok(client)
    }

    pub fn connection_state(&self) -> ClientConnectionState {
        *self.connection_state.lock().unwrap()
    }

    /// Subscribe to connection lifecycle changes. The callback is isolated
    /// from client state and may safely observe the transition asynchronously.
    pub fn subscribe_connection_state(
        &self,
        listener: impl Fn(&ConnectionStateChange) + Send + Sync + 'static,
    ) -> ConnectionStateUnsubscribe {
        let mut listeners = self.connection_state_listeners.lock().unwrap();
        let listener: ConnectionStateListener = Arc::new(listener);
        listeners.push(Some(listener));
        let index = listeners.len() - 1;
        let listeners = self.connection_state_listeners.clone();
        Box::new(move || {
            if let Some(slot) = listeners.lock().unwrap().get_mut(index) {
                *slot = None;
            }
        })
    }

    /// Reconnect the saved Unix transport and perform a fresh handshake.
    /// Pending requests from the previous connection are rejected when that
    /// connection closes; session handles are invalidated and must reattach.
    pub async fn reconnect(&self) -> Result<ServerSnapshot, PiClientError> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(PiClientError {
                message: "PiClient is disposed".into(),
            });
        }
        if self.connection_state() != ClientConnectionState::Disconnected {
            return Err(PiClientError {
                message: format!(
                    "PiClient is already {}",
                    state_name(self.connection_state())
                ),
            });
        }

        self.set_connection_state(ClientConnectionState::Connecting, None);
        *self.snapshot.lock().unwrap() = None;
        self.session_snapshots.lock().unwrap().clear();
        self.timed_out.lock().unwrap().clear();
        let epoch = self.connection_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let stream = match tokio::time::timeout(
            self.handshake_timeout,
            tokio::net::UnixStream::connect(self.socket_path.as_str()),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                let error = PiClientError {
                    message: format!("connect: {error}"),
                };
                self.connection_lost(epoch, error.clone());
                return Err(error);
            }
            Err(_) => {
                let error = PiClientError {
                    message: format!(
                        "connect timed out after {}ms",
                        self.handshake_timeout.as_millis()
                    ),
                };
                self.connection_lost(epoch, error.clone());
                return Err(error);
            }
        };
        let (connection, reader) = transport::ClientConnection::new(stream);
        *self.connection.lock().unwrap() = Some(connection.clone());

        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();
        let handshake: HandshakeWaiter = Arc::new(Mutex::new(Some(handshake_tx)));
        self.spawn_reader(epoch, reader, handshake.clone());
        if let Err(error) = connection
            .send_client_message(&ClientMessage::Hello {
                version: pi_protocol::PROTOCOL_VERSION,
            })
            .await
        {
            self.connection_lost(epoch, error.clone());
            return Err(error);
        }

        match tokio::time::timeout(self.handshake_timeout, handshake_rx).await {
            Ok(Ok(Ok(snapshot))) => {
                self.set_connection_state(ClientConnectionState::Connected, None);
                Ok(snapshot)
            }
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => {
                let error = PiClientError {
                    message: "connection closed during handshake".into(),
                };
                self.connection_lost(epoch, error.clone());
                Err(error)
            }
            Err(_) => {
                let error = PiClientError {
                    message: format!(
                        "handshake timed out after {}ms",
                        self.handshake_timeout.as_millis()
                    ),
                };
                self.connection_lost(epoch, error.clone());
                Err(error)
            }
        }
    }

    /// Send a command and await its correlated response.
    pub async fn request(&self, command: Command) -> Result<CommandResult, PiClientError> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(PiClientError {
                message: "PiClient is disposed".into(),
            });
        }
        if self.connection_state() != ClientConnectionState::Connected {
            return Err(PiClientError {
                message: "client is disconnected".into(),
            });
        }
        let epoch = self.connection_epoch.load(Ordering::SeqCst);
        let connection = self
            .connection
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| PiClientError {
                message: "client is disconnected".into(),
            })?;
        let id = format!("r{}", self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            pending.insert(id.clone(), Some(Pending { resolve: tx }));
        }
        let message = ClientMessage::Request {
            id,
            request: command,
        };
        if let Err(error) = connection.send_client_message(&message).await {
            self.pending.lock().unwrap().remove(&message_id(&message));
            self.connection_lost(epoch, error.clone());
            return Err(error);
        }
        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PiClientError {
                message: "connection closed before response".into(),
            }),
            Err(_) => {
                self.pending.lock().unwrap().remove(&message_id(&message));
                self.timed_out.lock().unwrap().insert(message_id(&message));
                Err(PiClientError {
                    message: format!(
                        "request timed out after {}ms",
                        self.request_timeout.as_millis()
                    ),
                })
            }
        }
    }

    /// Subscribe to server events.
    pub fn subscribe(&self, listener: EventListener) {
        self.listeners.lock().unwrap().push(listener);
    }

    pub fn snapshot(&self) -> Option<ServerSnapshot> {
        self.snapshot.lock().unwrap().clone()
    }

    /// Most recent `SessionSnapshot` observed for the given session id.
    /// Immediately record a session snapshot (used by the session-handle
    /// attach path so `handle.snapshot()` is correct before the event fanout
    /// arrives from the server reader task).
    pub(crate) fn note_session_snapshot(&self, snapshot: pi_protocol::SessionSnapshot) {
        self.session_snapshots
            .lock()
            .unwrap()
            .insert(snapshot.id.clone(), snapshot);
    }

    pub fn session_snapshot(&self, session_id: &str) -> Option<pi_protocol::SessionSnapshot> {
        self.session_snapshots
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
    }

    pub async fn close(&self) -> Result<(), PiClientError> {
        let error = PiClientError {
            message: "Client disconnected".into(),
        };
        let connection = self.connection.lock().unwrap().take();
        self.reject_pending(error.clone());
        self.set_connection_state(ClientConnectionState::Disconnected, Some(error));
        if let Some(connection) = connection {
            connection.close().await?;
        }
        Ok(())
    }

    /// Permanently dispose the client and release its listeners/state.
    /// Unlike `close`, a disposed client cannot reconnect or issue requests.
    pub async fn dispose(&self) -> Result<(), PiClientError> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.close().await?;
        self.listeners.lock().unwrap().clear();
        self.connection_state_listeners.lock().unwrap().clear();
        self.snapshot.lock().unwrap().take();
        self.session_snapshots.lock().unwrap().clear();
        Ok(())
    }

    fn spawn_reader(
        &self,
        epoch: u64,
        mut reader: tokio::net::unix::OwnedReadHalf,
        handshake: HandshakeWaiter,
    ) {
        let client = self.clone();
        tokio::spawn(async move {
            let mut decoder =
                pi_protocol::ServerMessageDecoder::new(&Default::default()).expect("decoder");
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                use tokio::io::AsyncReadExt;
                let n = match reader.read(&mut buf).await {
                    Ok(0) => {
                        client.connection_lost(
                            epoch,
                            PiClientError {
                                message: "connection closed by server".into(),
                            },
                        );
                        return;
                    }
                    Ok(n) => n,
                    Err(error) => {
                        client.connection_lost(
                            epoch,
                            PiClientError {
                                message: format!("read: {error}"),
                            },
                        );
                        return;
                    }
                };
                let messages = match decoder.push(&buf[..n]) {
                    Ok(messages) => messages,
                    Err(error) => {
                        client.connection_lost(
                            epoch,
                            PiClientError {
                                message: format!("decode: {error}"),
                            },
                        );
                        return;
                    }
                };
                for message in messages {
                    client.handle_message(message, &handshake, epoch);
                    if client.connection_state() == ClientConnectionState::Disconnected {
                        return;
                    }
                }
            }
        });
    }

    fn handle_message(&self, message: ServerMessage, handshake: &HandshakeWaiter, epoch: u64) {
        if self.connection_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        match message {
            ServerMessage::Hello { snapshot, .. } => {
                *self.snapshot.lock().unwrap() = Some(snapshot.clone());
                if let Some(sender) = handshake.lock().unwrap().take() {
                    let _ = sender.send(Ok(snapshot));
                    self.set_connection_state(ClientConnectionState::Connected, None);
                } else {
                    self.connection_lost(
                        epoch,
                        PiClientError {
                            message: "unexpected handshake message".into(),
                        },
                    );
                }
            }
            ServerMessage::HelloError { error } => {
                let error = PiClientError {
                    message: format!("{:?}: {}", error.code, error.message),
                };
                if let Some(sender) = handshake.lock().unwrap().take() {
                    let _ = sender.send(Err(error.clone()));
                }
                self.connection_lost(epoch, error);
            }
            ServerMessage::Response {
                id,
                ok,
                result,
                error,
            } => {
                let pending = self.pending.lock().unwrap().remove(&id);
                if pending.is_none() && self.timed_out.lock().unwrap().remove(&id) {
                    return;
                }
                let Some(Some(Pending { resolve })) = pending else {
                    self.connection_lost(
                        epoch,
                        PiClientError {
                            message: format!("response has no matching request: {id}"),
                        },
                    );
                    return;
                };
                let outcome = if ok {
                    result.map(Ok).unwrap_or_else(|| {
                        Err(PiClientError {
                            message: "response with no result".into(),
                        })
                    })
                } else {
                    let error = error.unwrap_or(pi_protocol::ProtocolError {
                        code: pi_protocol::ProtocolErrorCode::InternalError,
                        message: "unknown error".into(),
                        details: None,
                    });
                    Err(PiClientError {
                        message: format!("{:?}: {}", error.code, error.message),
                    })
                };
                let _ = resolve.send(outcome);
            }
            ServerMessage::Event { event } => {
                if self.connection_state() != ClientConnectionState::Connected {
                    self.connection_lost(
                        epoch,
                        PiClientError {
                            message: "received event before handshake".into(),
                        },
                    );
                    return;
                }
                if let ServerEvent::ServerSnapshot { snapshot } = &event {
                    *self.snapshot.lock().unwrap() = Some(snapshot.clone());
                }
                if let ServerEvent::SessionSnapshot { snapshot } = &event {
                    self.session_snapshots
                        .lock()
                        .unwrap()
                        .insert(snapshot.id.clone(), snapshot.clone());
                }
                let listeners = self.listeners.lock().unwrap().clone();
                for listener in listeners {
                    listener(&event);
                }
            }
        }
    }

    fn connection_lost(&self, epoch: u64, error: PiClientError) {
        if self.connection_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        let connection = self.connection.lock().unwrap().take();
        self.reject_pending(error.clone());
        self.set_connection_state(ClientConnectionState::Disconnected, Some(error));
        if let Some(connection) = connection {
            tokio::spawn(async move {
                let _ = connection.close().await;
            });
        }
    }

    fn reject_pending(&self, error: PiClientError) {
        let all_pending = std::mem::take(&mut *self.pending.lock().unwrap());
        self.timed_out.lock().unwrap().clear();
        for (_, pending) in all_pending {
            if let Some(Pending { resolve }) = pending {
                let _ = resolve.send(Err(error.clone()));
            }
        }
    }

    fn set_connection_state(&self, state: ClientConnectionState, error: Option<PiClientError>) {
        let changed = {
            let mut current = self.connection_state.lock().unwrap();
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        };
        if !changed {
            return;
        }
        let change = ConnectionStateChange { state, error };
        let listeners = self.connection_state_listeners.lock().unwrap().clone();
        for listener in listeners.into_iter().flatten() {
            listener(&change);
        }
    }
}

fn message_id(message: &ClientMessage) -> String {
    match message {
        ClientMessage::Request { id, .. } => id.clone(),
        ClientMessage::Hello { .. } => String::new(),
    }
}

fn state_name(state: ClientConnectionState) -> &'static str {
    match state {
        ClientConnectionState::Disconnected => "disconnected",
        ClientConnectionState::Connecting => "connecting",
        ClientConnectionState::Connected => "connected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    fn socket_path(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pi-client-{test_name}-{}.sock",
            uuid::Uuid::new_v4()
        ))
    }

    fn snapshot(revision: i64) -> ServerSnapshot {
        ServerSnapshot {
            server_id: "test-server".into(),
            protocol_version: pi_protocol::PROTOCOL_VERSION,
            revision,
            sessions: vec![],
            models: vec![],
        }
    }

    async fn read_client_message(reader: &mut tokio::net::unix::OwnedReadHalf) -> ClientMessage {
        let mut decoder = pi_protocol::ClientMessageDecoder::new(&Default::default()).unwrap();
        let mut buf = [0u8; 1024];
        loop {
            let count = reader.read(&mut buf).await.unwrap();
            assert!(count > 0, "client closed before sending a message");
            let messages = decoder.push(&buf[..count]).unwrap();
            if let Some(message) = messages.into_iter().next() {
                return message;
            }
        }
    }

    async fn send_server_message(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        message: ServerMessage,
    ) {
        let frame = pi_protocol::encode_server_message(&message, &Default::default()).unwrap();
        writer.write_all(&frame).await.unwrap();
        writer.flush().await.unwrap();
    }

    #[test]
    fn state_names_are_stable() {
        assert_eq!(
            state_name(ClientConnectionState::Disconnected),
            "disconnected"
        );
        assert_eq!(state_name(ClientConnectionState::Connecting), "connecting");
        assert_eq!(state_name(ClientConnectionState::Connected), "connected");
    }

    #[tokio::test]
    async fn reconnect_refreshes_snapshot_and_notifies_lifecycle() {
        let path = socket_path("reconnect");
        let listener = UnixListener::bind(&path).unwrap();
        let (drop_first_tx, drop_first_rx) = tokio::sync::oneshot::channel();
        let (second_ready_tx, second_ready_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            assert!(matches!(
                read_client_message(&mut reader).await,
                ClientMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION
                }
            ));
            send_server_message(
                &mut writer,
                ServerMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION,
                    connection_id: "first".into(),
                    snapshot: snapshot(1),
                },
            )
            .await;
            drop_first_rx.await.unwrap();
            drop(reader);
            drop(writer);

            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            assert!(matches!(
                read_client_message(&mut reader).await,
                ClientMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION
                }
            ));
            send_server_message(
                &mut writer,
                ServerMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION,
                    connection_id: "second".into(),
                    snapshot: snapshot(2),
                },
            )
            .await;
            second_ready_tx.send(()).unwrap();
            done_rx.await.unwrap();
        });

        let client = PiClient::connect_with_timeouts(
            path.to_str().unwrap(),
            Duration::from_secs(1),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(client.snapshot().unwrap().revision, 1);

        let states = Arc::new(Mutex::new(Vec::new()));
        let states_for_listener = states.clone();
        let unsubscribe = client.subscribe_connection_state(move |change| {
            states_for_listener.lock().unwrap().push(change.state);
        });

        drop_first_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if client.connection_state() == ClientConnectionState::Disconnected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let refreshed = client.reconnect().await.unwrap();
        second_ready_rx.await.unwrap();
        assert_eq!(refreshed.revision, 2);
        assert_eq!(client.snapshot().unwrap().revision, 2);
        assert_eq!(client.connection_state(), ClientConnectionState::Connected);

        let observed = states.lock().unwrap().clone();
        assert!(observed.contains(&ClientConnectionState::Disconnected));
        assert!(observed.contains(&ClientConnectionState::Connecting));
        assert!(observed.contains(&ClientConnectionState::Connected));

        unsubscribe();
        done_tx.send(()).unwrap();
        client.close().await.unwrap();
        client.dispose().await.unwrap();
        assert!(client
            .reconnect()
            .await
            .unwrap_err()
            .message
            .contains("disposed"));
        assert!(client
            .request(Command::List)
            .await
            .unwrap_err()
            .message
            .contains("disposed"));
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn handshake_timeout_returns_to_disconnected() {
        let path = socket_path("handshake-timeout");
        let listener = UnixListener::bind(&path).unwrap();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            done_rx.await.unwrap();
            drop(stream);
        });

        let result = PiClient::connect_with_timeouts(
            path.to_str().unwrap(),
            Duration::from_millis(20),
            Duration::from_millis(50),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("handshake unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.message.contains("handshake timed out"));

        done_tx.send(()).unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn request_timeout_does_not_leave_pending_request() {
        let path = socket_path("request-timeout");
        let listener = UnixListener::bind(&path).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (late_response_tx, late_response_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            assert!(matches!(
                read_client_message(&mut reader).await,
                ClientMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION
                }
            ));
            send_server_message(
                &mut writer,
                ServerMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION,
                    connection_id: "request-timeout".into(),
                    snapshot: snapshot(1),
                },
            )
            .await;
            ready_tx.send(()).unwrap();
            let request = read_client_message(&mut reader).await;
            let request_id = match request {
                ClientMessage::Request { id, .. } => id,
                ClientMessage::Hello { .. } => panic!("expected request after handshake"),
            };
            late_response_rx.await.unwrap();
            send_server_message(
                &mut writer,
                ServerMessage::Response {
                    id: request_id,
                    ok: true,
                    result: Some(CommandResult::List { sessions: vec![] }),
                    error: None,
                },
            )
            .await;
            done_rx.await.unwrap();
        });

        let client = PiClient::connect_with_timeouts(
            path.to_str().unwrap(),
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        ready_rx.await.unwrap();

        let error = client.request(Command::List).await.unwrap_err();
        assert!(error.message.contains("request timed out"));
        assert!(client.pending.lock().unwrap().is_empty());
        late_response_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(client.connection_state(), ClientConnectionState::Connected);

        done_tx.send(()).unwrap();
        client.close().await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }
}
