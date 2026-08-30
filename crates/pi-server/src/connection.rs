//! Byte connection abstraction — port of `packages/server/src/connection.ts`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_PENDING_BYTES: usize = pi_protocol::DEFAULT_MAX_FRAME_LENGTH * 4;
const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Requests received in the same transport turn as hello are held until
    /// the handshake has sent its server hello, matching the upstream queue.
    pub pending_requests: Vec<(String, pi_protocol::Command)>,
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
            pending_requests: Vec::new(),
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
    read_cancel: CancellationToken,
    writes: mpsc::UnboundedSender<WriteRequest>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    pending_bytes: Arc<std::sync::atomic::AtomicUsize>,
    max_pending_bytes: usize,
    close_state: Arc<CloseState>,
}

impl UnixByteConnection {
    pub fn from_parts(read_half: UnixReadHalf, write_half: UnixWriteHalf) -> Arc<Self> {
        Self::from_parts_with_max_pending_bytes(read_half, write_half, DEFAULT_MAX_PENDING_BYTES)
    }

    pub fn from_parts_with_max_pending_bytes(
        read_half: UnixReadHalf,
        write_half: UnixWriteHalf,
        max_pending_bytes: usize,
    ) -> Arc<Self> {
        assert!(max_pending_bytes > 0, "max_pending_bytes must be positive");
        let (writes, requests) = mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closing = Arc::new(AtomicBool::new(false));
        let pending_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let close_state = Arc::new(CloseState::default());
        let read_cancel = CancellationToken::new();
        let writer_task = tokio::spawn(write_loop(
            write_half,
            requests,
            closed.clone(),
            closing.clone(),
            pending_bytes.clone(),
            close_state.clone(),
        ));
        Arc::new(Self {
            read_half: std::sync::Mutex::new(Some(read_half)),
            read_cancel,
            writes,
            writer_task: Mutex::new(Some(writer_task)),
            closed,
            closing,
            pending_bytes,
            max_pending_bytes,
            close_state,
        })
    }

    pub(crate) fn read_cancel_token(&self) -> CancellationToken {
        self.read_cancel.clone()
    }
}

impl ByteConnection for UnixByteConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn take_reader(&self) -> Option<UnixReadHalf> {
        self.read_half
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn send(&self, chunk: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        if self.closed() || self.closing.load(Ordering::SeqCst) {
            return Box::pin(async { Err("Unix connection is closed".to_string()) });
        }
        if chunk.len() > self.max_pending_bytes {
            return Box::pin(async {
                Err("Unix connection exceeded its pending byte limit".to_string())
            });
        }
        let length = chunk.len();
        let mut current = self.pending_bytes.load(Ordering::SeqCst);
        loop {
            if current > self.max_pending_bytes - length {
                return Box::pin(async {
                    Err("Unix connection exceeded its pending byte limit".to_string())
                });
            }
            match self.pending_bytes.compare_exchange(
                current,
                current + length,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(updated) => current = updated,
            }
        }
        let (completion, receive) = oneshot::channel();
        if self
            .writes
            .send(WriteRequest::Data {
                chunk: chunk.to_vec(),
                length,
                completion,
            })
            .is_err()
        {
            self.pending_bytes.fetch_sub(length, Ordering::SeqCst);
            return Box::pin(async { Err("Unix connection is closed".to_string()) });
        }
        Box::pin(async move {
            receive
                .await
                .unwrap_or_else(|_| Err("Unix connection is closed".to_string()))
        })
    }

    fn close(
        &self,
        final_chunk: Option<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        // Closing the write half must also stop the transport read task. A
        // split Tokio UnixStream does not otherwise wake its OwnedReadHalf
        // when the peer is still connected, which would leak accepted
        // handlers on listener/server shutdown.
        self.read_cancel.cancel();
        let first = !self.closing.swap(true, Ordering::SeqCst);
        let close_state = self.close_state.clone();
        if first {
            let (completion, receive) = oneshot::channel();
            if self
                .writes
                .send(WriteRequest::Close {
                    chunk: final_chunk,
                    completion,
                })
                .is_err()
            {
                self.closed.store(true, Ordering::SeqCst);
                close_state.complete(Err("Unix connection is closed".to_string()));
            } else {
                let close_state_for_wait = close_state.clone();
                let writer_task = self
                    .writer_task
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                tokio::spawn(async move {
                    let result = match tokio::time::timeout(GRACEFUL_CLOSE_TIMEOUT, receive).await {
                        Ok(Ok(result)) => result,
                        Ok(Err(_)) => Err("Unix connection is closed".to_string()),
                        Err(_) => {
                            if let Some(writer_task) = writer_task {
                                writer_task.abort();
                            }
                            Err("Unix connection close timed out".to_string())
                        }
                    };
                    close_state_for_wait.complete(result);
                });
            }
        }
        Box::pin(async move { close_state.wait().await })
    }
}

#[derive(Default)]
struct CloseState {
    result: Mutex<Option<Result<(), String>>>,
    notify: Notify,
}

impl CloseState {
    fn complete(&self, result: Result<(), String>) {
        let mut stored = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if stored.is_none() {
            *stored = Some(result);
            self.notify.notify_waiters();
        }
    }

    async fn wait(&self) -> Result<(), String> {
        loop {
            let notified = self.notify.notified();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

enum WriteRequest {
    Data {
        chunk: Vec<u8>,
        length: usize,
        completion: oneshot::Sender<Result<(), String>>,
    },
    Close {
        chunk: Option<Vec<u8>>,
        completion: oneshot::Sender<Result<(), String>>,
    },
}

async fn write_loop(
    mut writer: UnixWriteHalf,
    mut requests: mpsc::UnboundedReceiver<WriteRequest>,
    closed: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    pending_bytes: Arc<std::sync::atomic::AtomicUsize>,
    close_state: Arc<CloseState>,
) {
    use tokio::io::AsyncWriteExt;

    while let Some(request) = requests.recv().await {
        match request {
            WriteRequest::Data {
                chunk,
                length,
                completion,
            } => {
                let result = if closed.load(Ordering::SeqCst) {
                    Err("Unix connection is closed".to_string())
                } else {
                    let result = writer.write_all(&chunk).await;
                    if let Err(error) = result {
                        Err(format!("Unix connection closed during write: {error}"))
                    } else {
                        writer.flush().await.map_err(|error| {
                            format!("Unix connection closed during write: {error}")
                        })
                    }
                };
                pending_bytes.fetch_sub(length, Ordering::SeqCst);
                if result.is_err() {
                    closed.store(true, Ordering::SeqCst);
                    closing.store(true, Ordering::SeqCst);
                }
                let failed = result.is_err();
                let _ = completion.send(result);
                if failed {
                    close_state.complete(Err(
                        "Unix connection closed during write; pending output was discarded"
                            .to_string(),
                    ));
                    while let Ok(request) = requests.try_recv() {
                        match request {
                            WriteRequest::Data {
                                length, completion, ..
                            } => {
                                pending_bytes.fetch_sub(length, Ordering::SeqCst);
                                let _ =
                                    completion.send(Err("Unix connection is closed".to_string()));
                            }
                            WriteRequest::Close { completion, .. } => {
                                let _ =
                                    completion.send(Err("Unix connection is closed".to_string()));
                            }
                        }
                    }
                    break;
                }
            }
            WriteRequest::Close { chunk, completion } => {
                let result = if closed.load(Ordering::SeqCst) {
                    Err("Unix connection is closed".to_string())
                } else {
                    let result = async {
                        if let Some(chunk) = chunk {
                            writer.write_all(&chunk).await?;
                            writer.flush().await?;
                        }
                        writer.shutdown().await
                    }
                    .await
                    .map_err(|error| format!("Unix connection close failed: {error}"));
                    result
                };
                closed.store(true, Ordering::SeqCst);
                let _ = completion.send(result);
                break;
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{ByteConnection, UnixByteConnection};
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn unix_connection_preserves_send_invocation_order() {
        let (server, client) = tokio::net::UnixStream::pair().expect("UnixStream pair");
        let (server_read, server_write) = server.into_split();
        let (mut client_read, _client_write) = client.into_split();
        let connection = UnixByteConnection::from_parts(server_read, server_write);

        let first = connection.send(b"first");
        let second = connection.send(b"second");
        assert_eq!(first.await, Ok(()));
        assert_eq!(second.await, Ok(()));

        let mut received = [0u8; 11];
        client_read.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"firstsecond");
        connection.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn unix_connection_drains_queued_bytes_before_final_close_chunk() {
        let (server, client) = tokio::net::UnixStream::pair().expect("UnixStream pair");
        let (server_read, server_write) = server.into_split();
        let (mut client_read, _client_write) = client.into_split();
        let connection = UnixByteConnection::from_parts(server_read, server_write);

        let first = connection.send(b"queued");
        let close = connection.close(Some(b"final".to_vec()));
        assert_eq!(first.await, Ok(()));
        assert_eq!(close.await, Ok(()));

        let mut received = [0u8; 11];
        client_read.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"queuedfinal");
    }

    #[tokio::test]
    async fn unix_connection_rejects_a_chunk_over_the_pending_limit() {
        let (server, _client) = tokio::net::UnixStream::pair().expect("UnixStream pair");
        let (server_read, server_write) = server.into_split();
        let connection =
            UnixByteConnection::from_parts_with_max_pending_bytes(server_read, server_write, 4);

        let error = connection.send(b"12345").await.unwrap_err();
        assert!(error.contains("pending byte limit"));
        connection.close(None).await.unwrap();
    }
}
