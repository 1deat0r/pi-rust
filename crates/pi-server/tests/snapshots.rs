#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_protocol::{FrameDecoderOptions, ServerEvent, ServerMessage, ServerMessageDecoder};
use pi_server::{ByteConnection, ServerSnapshotPublisher};

struct RecordingConnection {
    closed: AtomicBool,
    fail_send: AtomicBool,
    frames: Mutex<Vec<Vec<u8>>>,
}

impl RecordingConnection {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            fail_send: AtomicBool::new(false),
            frames: Mutex::new(Vec::new()),
        })
    }
}

impl ByteConnection for RecordingConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn send(&self, chunk: &[u8]) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let closed = &self.closed;
        let fail_send = &self.fail_send;
        let frames = &self.frames;
        let chunk = chunk.to_vec();
        Box::pin(async move {
            if closed.load(Ordering::SeqCst) || fail_send.load(Ordering::SeqCst) {
                return Err("recording connection failed".to_string());
            }
            frames.lock().unwrap().push(chunk);
            Ok(())
        })
    }

    fn close(
        &self,
        _final_chunk: Option<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.closed.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn snapshot_publisher_deduplicates_and_evicts_failed_connections() {
    let publisher = ServerSnapshotPublisher::new(
        "server-1".to_string(),
        pi_protocol::PROTOCOL_VERSION,
        vec![],
    );
    let recording = RecordingConnection::new();
    let connection: Arc<dyn ByteConnection> = recording.clone();
    publisher.register_connection(connection.clone());
    publisher.register_connection(connection.clone());

    publisher.broadcast().await;
    let frames = recording.frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    let mut decoder = ServerMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
    let messages = decoder.push(&frames[0]).unwrap();
    assert!(matches!(
        messages.first(),
        Some(ServerMessage::Event {
            event: ServerEvent::ServerSnapshot { .. }
        })
    ));

    // A failed send is removed from the publisher registry. A later healthy
    // broadcast therefore cannot keep retrying a dead recipient.
    recording.fail_send.store(true, Ordering::SeqCst);
    publisher.broadcast().await;
    recording.fail_send.store(false, Ordering::SeqCst);
    publisher.broadcast().await;
    assert_eq!(recording.frames.lock().unwrap().len(), 1);
}
