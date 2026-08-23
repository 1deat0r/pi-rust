//! Byte connection abstraction — port of `packages/server/src/connection.ts`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub type UnixReadHalf = tokio::net::unix::OwnedReadHalf;
pub type UnixWriteHalf = tokio::net::unix::OwnedWriteHalf;

/// An established, authorized ordered byte connection. Send/close take
/// `&self`; transports use interior mutability so connections can be shared
/// behind `Arc<dyn ByteConnection>`.
pub trait ByteConnection: Send + Sync {
    fn closed(&self) -> bool;
    fn send(&self, chunk: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
    fn close(
        &self,
        final_chunk: Option<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
    /// Transport-owned read half for the accept loop's read task (default: none).
    fn take_reader(&self) -> Option<UnixReadHalf> {
        None
    }
}

/// Connection event handler. `on_data` receives the shared handler Arc so the
/// handler can spawn async work (request dispatch) while remaining behind a
/// mutex.
pub trait ByteConnectionHandler: Send {
    fn on_data(&mut self, chunk: &[u8], self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>);
    fn on_close(&mut self);
    fn on_error(&mut self, error: String);
    /// Downcast to the concrete connection handler for async dispatch
    /// (implemented by the server's handler; default returns None).
    fn as_connection_handler(&mut self) -> Option<&mut crate::server::ConnectionHandler> {
        None
    }
}

/// Accept a raw byte connection and return its handler, shared for the read
/// loop and the server's send path.
pub type ByteConnectionAcceptor =
    Arc<dyn Fn(Arc<dyn ByteConnection>) -> Arc<Mutex<dyn ByteConnectionHandler>> + Send + Sync>;

pub type ConnectionStage = &'static str;

pub struct ConnectionState {
    pub id: String,
    pub connection: Arc<dyn ByteConnection>,
    pub decoder: pi_protocol::ClientMessageDecoder,
    pub session_ids: std::collections::HashSet<String>,
    pub stage: ConnectionStage,
    pub disconnected: bool,
    pub handshake_complete: bool,
}

impl ConnectionState {
    pub fn new(
        id: String,
        connection: Arc<dyn ByteConnection>,
        decoder: pi_protocol::ClientMessageDecoder,
    ) -> Self {
        Self {
            id,
            connection,
            decoder,
            session_ids: Default::default(),
            stage: "awaitingHello",
            disconnected: false,
            handshake_complete: false,
        }
    }
}

pub fn is_terminal_connection(state: &ConnectionState) -> bool {
    state.disconnected || state.stage == "closing" || state.stage == "closed"
}

/// A Unix-stream-backed byte connection (write half; the read loop owns the
/// read half).
pub struct UnixByteConnection {
    read_half: std::sync::Mutex<Option<UnixReadHalf>>,
    writer: tokio::sync::Mutex<UnixWriteHalf>,
    closed: Arc<AtomicBool>,
}

impl UnixByteConnection {
    pub fn from_parts(read_half: UnixReadHalf, write_half: UnixWriteHalf) -> Arc<Self> {
        Arc::new(Self {
            read_half: std::sync::Mutex::new(Some(read_half)),
            writer: tokio::sync::Mutex::new(write_half),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl ByteConnection for UnixByteConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn take_reader(&self) -> Option<UnixReadHalf> {
        self.read_half.lock().unwrap().take()
    }

    fn send(&self, chunk: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let owned = chunk.to_vec();
        Box::pin(async move { self.send_inner(&owned).await })
    }

    fn close(
        &self,
        final_chunk: Option<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move { self.close_inner(final_chunk).await })
    }
}

impl UnixByteConnection {
    async fn send_inner(&self, chunk: &[u8]) -> Result<(), String> {
        if self.closed() {
            return Err("Unix connection is closed".to_string());
        }
        use tokio::io::AsyncWriteExt;
        let mut writer = self.writer.lock().await;
        if writer.write_all(chunk).await.is_err() {
            self.closed.store(true, Ordering::SeqCst);
            return Err("Unix connection closed during write".to_string());
        }
        writer.flush().await.map_err(|e| e.to_string())
    }

    async fn close_inner(&self, final_chunk: Option<Vec<u8>>) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut writer = self.writer.lock().await;
        if let Some(chunk) = final_chunk {
            let _ = writer.write_all(&chunk).await;
            let _ = writer.flush().await;
        }
        let _ = writer.shutdown().await;
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
