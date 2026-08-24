//! Client byte transports and transport factories.
//!
//! The protocol connection owns framing and lifecycle state. A transport only
//! moves arbitrary byte chunks and reports terminal input, which keeps the
//! client testable without requiring a Unix socket and leaves non-Unix or
//! embedded transports behind the same async factory seam as upstream.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::PiClientError;

/// Callbacks supplied to a newly-created byte transport.
#[derive(Clone)]
pub struct TransportHandlers {
    /// Delivers one arbitrary inbound byte chunk.
    pub on_data: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    /// Reports an orderly terminal close.
    pub on_close: Arc<dyn Fn() + Send + Sync>,
    /// Reports a terminal transport failure.
    pub on_error: Arc<dyn Fn(PiClientError) + Send + Sync>,
}

impl TransportHandlers {
    pub fn new(
        on_data: impl Fn(Vec<u8>) + Send + Sync + 'static,
        on_close: impl Fn() + Send + Sync + 'static,
        on_error: impl Fn(PiClientError) + Send + Sync + 'static,
    ) -> Self {
        Self {
            on_data: Arc::new(on_data),
            on_close: Arc::new(on_close),
            on_error: Arc::new(on_error),
        }
    }
}

/// A connected byte stream. Calls to `send` must complete in invocation order.
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ByteTransport: Send + Sync {
    fn send<'a>(&'a self, chunk: Vec<u8>) -> TransportFuture<'a, Result<(), PiClientError>>;

    /// Close is terminal and must be safe to call more than once.
    fn close(&self);
}

/// Creates a fresh connected transport for each client connection attempt.
pub trait TransportFactory: Send + Sync {
    fn connect<'a>(
        &'a self,
        handlers: TransportHandlers,
    ) -> TransportFuture<'a, Result<Arc<dyn ByteTransport>, PiClientError>>;
}

impl<T> TransportFactory for Arc<T>
where
    T: TransportFactory + ?Sized,
{
    fn connect<'a>(
        &'a self,
        handlers: TransportHandlers,
    ) -> TransportFuture<'a, Result<Arc<dyn ByteTransport>, PiClientError>> {
        Box::pin(async move { (**self).connect(handlers).await })
    }
}

/// Unix-domain socket factory used by the compatibility `PiClient::connect`
/// constructor and by real-socket parity fixtures.
#[derive(Clone, Debug)]
pub struct UnixTransportFactory {
    path: PathBuf,
    max_pending_bytes: usize,
}

impl UnixTransportFactory {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, PiClientError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(PiClientError {
                message: "Unix transport path must not be empty".into(),
            });
        }
        if path.to_string_lossy().len() > 107 {
            return Err(PiClientError {
                message: "Unix transport path is too long; maximum is 107 bytes".into(),
            });
        }
        Ok(Self {
            path,
            max_pending_bytes: pi_protocol::DEFAULT_MAX_FRAME_LENGTH * 4,
        })
    }

    pub fn with_max_pending_bytes(
        mut self,
        max_pending_bytes: usize,
    ) -> Result<Self, PiClientError> {
        if max_pending_bytes == 0 {
            return Err(PiClientError {
                message: "Unix transport max_pending_bytes must be positive".into(),
            });
        }
        self.max_pending_bytes = max_pending_bytes;
        Ok(self)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl TransportFactory for UnixTransportFactory {
    fn connect<'a>(
        &'a self,
        handlers: TransportHandlers,
    ) -> TransportFuture<'a, Result<Arc<dyn ByteTransport>, PiClientError>> {
        Box::pin(async move {
            #[cfg(unix)]
            {
                let stream =
                    tokio::net::UnixStream::connect(&self.path)
                        .await
                        .map_err(|error| PiClientError {
                            message: format!("connect: {error}"),
                        })?;
                let (mut reader, writer) = stream.into_split();
                let closed = Arc::new(AtomicBool::new(false));
                let reader_closed = closed.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 64 * 1024];
                    loop {
                        use tokio::io::AsyncReadExt;
                        let count = match reader.read(&mut buffer).await {
                            Ok(count) => count,
                            Err(error) => {
                                if !reader_closed.swap(true, Ordering::SeqCst) {
                                    (handlers.on_error)(PiClientError {
                                        message: format!("read: {error}"),
                                    });
                                }
                                return;
                            }
                        };
                        if count == 0 {
                            if !reader_closed.swap(true, Ordering::SeqCst) {
                                (handlers.on_close)();
                            }
                            return;
                        }
                        if reader_closed.load(Ordering::SeqCst) {
                            return;
                        }
                        (handlers.on_data)(buffer[..count].to_vec());
                    }
                });
                let transport: Arc<dyn ByteTransport> = Arc::new(UnixByteTransport {
                    writer: Arc::new(tokio::sync::Mutex::new(writer)),
                    closed,
                    pending_bytes: Arc::new(AtomicUsize::new(0)),
                    max_pending_bytes: self.max_pending_bytes,
                });
                Ok(transport)
            }
            #[cfg(not(unix))]
            {
                let _ = handlers;
                Err(PiClientError {
                    message: "Unix transport is not supported on this platform".into(),
                })
            }
        })
    }
}

#[cfg(unix)]
struct UnixByteTransport {
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    closed: Arc<AtomicBool>,
    pending_bytes: Arc<AtomicUsize>,
    max_pending_bytes: usize,
}

#[cfg(unix)]
impl ByteTransport for UnixByteTransport {
    fn send<'a>(&'a self, chunk: Vec<u8>) -> TransportFuture<'a, Result<(), PiClientError>> {
        Box::pin(async move {
            if chunk.len() > self.max_pending_bytes {
                return Err(PiClientError {
                    message: "Unix transport exceeded its pending byte limit".into(),
                });
            }
            loop {
                let current = self.pending_bytes.load(Ordering::SeqCst);
                if current > self.max_pending_bytes - chunk.len() {
                    return Err(PiClientError {
                        message: "Unix transport exceeded its pending byte limit".into(),
                    });
                }
                if self
                    .pending_bytes
                    .compare_exchange(
                        current,
                        current + chunk.len(),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    break;
                }
            }

            let result = async {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(PiClientError {
                        message: "Unix transport is closed".into(),
                    });
                }
                use tokio::io::AsyncWriteExt;
                let mut writer = self.writer.lock().await;
                writer
                    .write_all(&chunk)
                    .await
                    .map_err(|error| PiClientError {
                        message: format!("write: {error}"),
                    })?;
                writer.flush().await.map_err(|error| PiClientError {
                    message: format!("flush: {error}"),
                })
            }
            .await;
            self.pending_bytes.fetch_sub(chunk.len(), Ordering::SeqCst);
            result
        })
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let writer = self.writer.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut writer = writer.lock().await;
            let _ = writer.shutdown().await;
        });
    }
}
