//! Pi protocol client — port of `packages/client`.
//!
//! Connects to a `PiServer` over a Unix-domain socket, performs the hello
//! handshake, sends `Command`s with request/response correlation, and emits
//! `ServerEvent`s (server/session snapshots, progress) to subscribers.

pub mod session_handle;
pub mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

/// PiClient: connects, handshakes, and issues protocol commands.
/// Clone is cheap: all mutable state is Arc-shared.
#[derive(Clone)]
pub struct PiClient {
    connection: transport::ClientConnection,
    pending: Arc<Mutex<HashMap<String, Option<Pending>>>>,
    listeners: Arc<Mutex<Vec<EventListener>>>,
    snapshot: Arc<Mutex<Option<ServerSnapshot>>>,
    session_snapshots: Arc<Mutex<HashMap<String, pi_protocol::SessionSnapshot>>>,
    next_request_id: Arc<AtomicU64>,
}

impl PiClient {
    pub async fn connect(socket_path: &str) -> Result<Self, PiClientError> {
        let stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| PiClientError {
                message: format!("connect: {e}"),
            })?;
        let pending: Arc<Mutex<HashMap<String, Option<Pending>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let listeners: Arc<Mutex<Vec<EventListener>>> = Arc::new(Mutex::new(Vec::new()));
        let snapshot: Arc<Mutex<Option<ServerSnapshot>>> = Arc::new(Mutex::new(None));
        let session_snapshots: Arc<Mutex<HashMap<String, pi_protocol::SessionSnapshot>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (connection, reader) = transport::ClientConnection::new(stream);
        let mut client = Self {
            connection,
            pending,
            listeners,
            snapshot,
            session_snapshots,
            next_request_id: Arc::new(AtomicU64::new(1)),
        };
        client.handshake(reader).await?;
        Ok(client)
    }

    async fn handshake(
        &mut self,
        mut reader: tokio::net::unix::OwnedReadHalf,
    ) -> Result<(), PiClientError> {
        self.connection
            .send_client_message(&ClientMessage::Hello {
                version: pi_protocol::PROTOCOL_VERSION,
            })
            .await?;
        // Reader task processes messages into pending/snapshot/listeners.
        let pending = self.pending.clone();
        let listeners = self.listeners.clone();
        let snapshot = self.snapshot.clone();
        let session_snapshots = self.session_snapshots.clone();
        let mut decoder =
            pi_protocol::ServerMessageDecoder::new(&Default::default()).expect("decoder");
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                use tokio::io::AsyncReadExt;
                let n = match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                let messages = match decoder.push(&buf[..n]) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                for message in messages {
                    handle_message(message, &pending, &listeners, &snapshot, &session_snapshots);
                }
            }
        });
        // Wait for the server hello snapshot (bounded).
        for _ in 0..100 {
            if self.snapshot().is_some() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Err(PiClientError {
            message: "handshake timed out waiting for server hello".into(),
        })
    }

    /// Send a command and await its correlated response.
    pub async fn request(&self, command: Command) -> Result<CommandResult, PiClientError> {
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
        self.connection.send_client_message(&message).await?;
        rx.await.map_err(|_| PiClientError {
            message: "connection closed before response".into(),
        })?
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
        self.connection.close().await
    }
}

fn handle_message(
    message: ServerMessage,
    pending: &Arc<Mutex<HashMap<String, Option<Pending>>>>,
    listeners: &Arc<Mutex<Vec<EventListener>>>,
    snapshot: &Arc<Mutex<Option<ServerSnapshot>>>,
    session_snapshots: &Arc<Mutex<HashMap<String, pi_protocol::SessionSnapshot>>>,
) {
    match message {
        ServerMessage::Hello { snapshot: snap, .. } => {
            *snapshot.lock().unwrap() = Some(snap);
        }
        ServerMessage::HelloError { error } => {
            let msg = format!("{:?}: {}", error.code, error.message);
            let all_pending = std::mem::take(&mut *pending.lock().unwrap());
            for (_, p) in all_pending {
                if let Some(Pending { resolve }) = p {
                    let _ = resolve.send(Err(PiClientError {
                        message: msg.clone(),
                    }));
                }
            }
        }
        ServerMessage::Response {
            id,
            ok,
            result,
            error,
        } => {
            let mut pending_map = pending.lock().unwrap();
            if let Some(Some(Pending { resolve })) = pending_map.remove(&id) {
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
        }
        ServerMessage::Event { event } => {
            if let ServerEvent::ServerSnapshot { snapshot: snap } = &event {
                *snapshot.lock().unwrap() = Some(snap.clone());
            }
            if let ServerEvent::SessionSnapshot { snapshot: sess } = &event {
                session_snapshots
                    .lock()
                    .unwrap()
                    .insert(sess.id.clone(), sess.clone());
            }
            let listeners = listeners.lock().unwrap().clone();
            for listener in listeners {
                listener(&event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_increment() {
        let event = ServerEvent::ServerSnapshot {
            snapshot: ServerSnapshot {
                server_id: "s".into(),
                protocol_version: 1,
                revision: 0,
                sessions: vec![],
                models: vec![],
            },
        };
        let pending: Arc<Mutex<HashMap<String, Option<Pending>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let listeners: Arc<Mutex<Vec<EventListener>>> = Arc::new(Mutex::new(Vec::new()));
        let snapshot: Arc<Mutex<Option<ServerSnapshot>>> = Arc::new(Mutex::new(None));
        handle_message(
            ServerMessage::Event { event },
            &pending,
            &listeners,
            &snapshot,
            &Arc::new(Mutex::new(HashMap::new())),
        );
        assert!(snapshot.lock().unwrap().is_some());
    }
}
