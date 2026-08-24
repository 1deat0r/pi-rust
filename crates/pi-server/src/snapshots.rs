//! Server snapshot publisher — port of `packages/server/src/snapshots.ts`.

use std::sync::{Arc, Mutex};

use pi_protocol::{ServerEvent, ServerMessage, ServerSnapshot, SessionMetadata, SessionSnapshot};

use crate::connection::ByteConnection;

/// Cached server snapshot metadata + revision, refreshed by the server after
/// each mutating command and broadcast on advancement.
pub struct ServerSnapshotPublisher {
    server_id: String,
    protocol_version: u64,
    revision: Arc<Mutex<i64>>,
    sessions: Arc<Mutex<Vec<SessionMetadata>>>,
    models: Arc<Mutex<Vec<pi_protocol::ModelMetadata>>>,
    connections: Arc<Mutex<Vec<Arc<dyn ByteConnection>>>>,
    broadcast_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ServerSnapshotPublisher {
    pub fn new(
        server_id: String,
        protocol_version: u64,
        models: Vec<pi_protocol::ModelMetadata>,
    ) -> Self {
        Self {
            server_id,
            protocol_version,
            revision: Arc::new(Mutex::new(0)),
            sessions: Arc::new(Mutex::new(Vec::new())),
            models: Arc::new(Mutex::new(models)),
            connections: Arc::new(Mutex::new(Vec::new())),
            broadcast_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Seed the cached session metadata without advancing the mutation
    /// revision. The upstream publisher starts at revision zero and obtains
    /// its initial session list during the hello snapshot.
    pub fn initialize(&self, sessions: Vec<SessionMetadata>) {
        *self.sessions.lock().unwrap() = sessions;
    }

    pub fn register_connection(&self, connection: Arc<dyn ByteConnection>) {
        self.connections.lock().unwrap().push(connection);
    }

    pub fn revoke_connection_for(&self, connection: &Arc<dyn ByteConnection>) {
        let mut conns = self.connections.lock().unwrap();
        conns.retain(|candidate| !Arc::ptr_eq(candidate, connection));
    }

    pub fn revoke_connection(&self, id: &str) {
        let _ = id;
        let mut conns = self.connections.lock().unwrap();
        conns.retain(|c| !c.closed());
    }

    pub fn current_revision(&self) -> i64 {
        *self.revision.lock().unwrap()
    }

    /// Refresh cached session metadata and advance the revision.
    pub fn refresh(&self, sessions: Vec<SessionMetadata>) {
        *self.sessions.lock().unwrap() = sessions;
        let mut revision = self.revision.lock().unwrap();
        *revision += 1;
    }

    pub fn get(&self) -> ServerSnapshot {
        ServerSnapshot {
            server_id: self.server_id.clone(),
            protocol_version: self.protocol_version,
            revision: self.current_revision(),
            sessions: self.sessions.lock().unwrap().clone(),
            models: self.models.lock().unwrap().clone(),
        }
    }

    /// Broadcast a per-session snapshot event to all connected clients
    /// (upstream Snapshots.publishSessionSnapshot).
    pub async fn broadcast_session_event(&self, session: SessionSnapshot) {
        let _broadcast_guard = self.broadcast_lock.lock().await;
        let conns = {
            let conns = self.connections.lock().unwrap().clone();
            if conns.is_empty() {
                return;
            }
            let message = ServerMessage::Event {
                event: ServerEvent::SessionSnapshot { snapshot: session },
            };
            let Ok(frame) = pi_protocol::encode_server_message(&message, &Default::default())
            else {
                return;
            };
            (frame, conns)
        };
        for conn in conns.1 {
            let _ = conn.send(&conns.0).await;
        }
    }

    pub async fn broadcast(&self) {
        let _broadcast_guard = self.broadcast_lock.lock().await;
        let snapshot = {
            let conns = self.connections.lock().unwrap().clone();
            if conns.is_empty() {
                return;
            }
            let message = ServerMessage::Event {
                event: ServerEvent::ServerSnapshot {
                    snapshot: self.get(),
                },
            };
            let Ok(frame) = pi_protocol::encode_server_message(&message, &Default::default())
            else {
                return;
            };
            // Return the frame plus the connection clones without holding
            // the registry lock across awaits.
            (frame, conns)
        };
        for conn in snapshot.1 {
            let _ = conn.send(&snapshot.0).await;
        }
    }
}
