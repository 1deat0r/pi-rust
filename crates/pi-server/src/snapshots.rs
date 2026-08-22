//! Server snapshot publisher — port of `packages/server/src/snapshots.ts`.

use std::sync::{Arc, Mutex};

use pi_protocol::{ServerEvent, ServerMessage, ServerSnapshot, SessionMetadata};

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
        }
    }

    pub fn register_connection(&self, connection: Arc<dyn ByteConnection>) {
        self.connections.lock().unwrap().push(connection);
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

    pub async fn broadcast(&self) {
        let snapshot = {
            let conns = self.connections.lock().unwrap().clone();
            if conns.is_empty() {
                return;
            }
            let message = ServerMessage::Event {
                event: ServerEvent::ServerSnapshot { snapshot: self.get() },
            };
            let Ok(frame) = pi_protocol::encode_server_message(&message, &Default::default()) else {
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
