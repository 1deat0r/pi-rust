//! Client transport over a Unix-domain socket (4-byte length framing +
//! CBOR message encoding, mirroring the server's `frame` + `MessageCodec`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use crate::PiClientError;

/// Owns the socket write half; the read half is passed to the reader task.
/// Clone is cheap: the writer and closed flag are Arc-shared.
#[derive(Clone)]
pub struct ClientConnection {
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    closed: Arc<AtomicBool>,
}

impl ClientConnection {
    pub fn new(stream: tokio::net::UnixStream) -> (Self, tokio::net::unix::OwnedReadHalf) {
        let (reader, writer) = stream.into_split();
        (
            Self {
                writer: Arc::new(tokio::sync::Mutex::new(writer)),
                closed: Arc::new(AtomicBool::new(false)),
            },
            reader,
        )
    }

    pub async fn send_client_message(
        &self,
        message: &pi_protocol::ClientMessage,
    ) -> Result<(), PiClientError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(PiClientError {
                message: "client is closed".into(),
            });
        }
        // encode_client_message returns a complete length-prefixed frame.
        let frame =
            pi_protocol::encode_client_message(message, &Default::default()).map_err(|e| {
                PiClientError {
                    message: format!("encode: {e}"),
                }
            })?;
        let mut writer = self.writer.lock().await;
        writer.write_all(&frame).await.map_err(|e| PiClientError {
            message: format!("write: {e}"),
        })?;
        writer.flush().await.map_err(|e| PiClientError {
            message: format!("flush: {e}"),
        })
    }

    pub async fn close(&self) -> Result<(), PiClientError> {
        use tokio::io::AsyncWriteExt;
        let mut writer = self.writer.lock().await;
        let _ = writer.shutdown().await;
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
