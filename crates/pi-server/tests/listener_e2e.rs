#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_server::{ByteConnectionAcceptor, ByteConnectionHandler, PiServerListener, UnixListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Notify;

struct RecordingHandler {
    closed: Arc<AtomicBool>,
    close_notify: Arc<Notify>,
}

impl ByteConnectionHandler for RecordingHandler {
    fn on_data(&mut self, _chunk: &[u8], _self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>) {}

    fn on_close(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.close_notify.notify_one();
        }
    }

    fn on_error(&mut self, _error: String) {}
}

fn recording_acceptor(
    accepted: Arc<AtomicUsize>,
    accepted_notify: Arc<Notify>,
    close_notify: Arc<Notify>,
) -> ByteConnectionAcceptor {
    Arc::new(move |_connection| {
        accepted.fetch_add(1, Ordering::SeqCst);
        accepted_notify.notify_waiters();
        Arc::new(Mutex::new(RecordingHandler {
            closed: Arc::new(AtomicBool::new(false)),
            close_notify: close_notify.clone(),
        }))
    })
}

async fn wait_for_count(accepted: &AtomicUsize, accepted_notify: &Notify, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        if accepted.load(Ordering::SeqCst) >= expected {
            return;
        }
        let notified = accepted_notify.notified();
        if accepted.load(Ordering::SeqCst) >= expected {
            return;
        }
        tokio::time::timeout_at(deadline, notified)
            .await
            .expect("timed out waiting for accepted Unix connection");
    }
}

fn test_socket_path(label: &str) -> (std::path::PathBuf, String) {
    let directory =
        std::env::temp_dir().join(format!("pi-server-{label}-{}", uuid::Uuid::new_v4()));
    let socket = directory.join("server.sock").to_string_lossy().into_owned();
    (directory, socket)
}

#[tokio::test]
async fn listener_close_terminates_accepted_connection_and_read_task() {
    let (directory, socket) = test_socket_path("close");
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_notify = Arc::new(Notify::new());
    let close_notify = Arc::new(Notify::new());
    let mut listener = UnixListener::new(socket.clone()).unwrap();
    listener
        .start(recording_acceptor(
            accepted.clone(),
            accepted_notify.clone(),
            close_notify.clone(),
        ))
        .await
        .unwrap();

    let client = UnixStream::connect(&socket).await.unwrap();
    wait_for_count(&accepted, &accepted_notify, 1).await;
    let (mut client_read, _client_write) = client.into_split();

    tokio::time::timeout(Duration::from_secs(1), listener.close())
        .await
        .expect("listener close timed out")
        .unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(1), client_read.read(&mut byte))
        .await
        .expect("client did not observe listener shutdown")
        .unwrap();
    assert_eq!(read, 0, "listener shutdown must EOF the accepted client");
    tokio::time::timeout(Duration::from_secs(1), close_notify.notified())
        .await
        .expect("read task did not observe listener shutdown");
    assert!(listener.address().is_none());
    assert!(tokio::fs::symlink_metadata(&socket).await.is_err());

    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn auth_isolated_connections_allow_progress_reject_bad_tokens_and_cancel_on_close() {
    let (directory, socket) = test_socket_path("auth");
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_notify = Arc::new(Notify::new());
    let close_notify = Arc::new(Notify::new());
    let mut listener = UnixListener::new(socket.clone())
        .unwrap()
        .with_auth_token("secret")
        .unwrap();
    listener
        .start(recording_acceptor(
            accepted.clone(),
            accepted_notify.clone(),
            close_notify.clone(),
        ))
        .await
        .unwrap();

    // A client that never sends its preface must not monopolize accept().
    let stalled = UnixStream::connect(&socket).await.unwrap();
    let mut valid = UnixStream::connect(&socket).await.unwrap();
    valid.write_all(b"PI-AUTH secret\n").await.unwrap();
    valid.flush().await.unwrap();
    wait_for_count(&accepted, &accepted_notify, 1).await;
    assert_eq!(accepted.load(Ordering::SeqCst), 1);

    let mut rejected = UnixStream::connect(&socket).await.unwrap();
    rejected.write_all(b"PI-AUTH nope__\n").await.unwrap();
    rejected.flush().await.unwrap();
    let (mut rejected_read, _rejected_write) = rejected.into_split();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(1), rejected_read.read(&mut byte))
        .await
        .expect("bad auth connection was not closed")
        .unwrap();
    assert_eq!(read, 0);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);

    tokio::time::timeout(Duration::from_secs(1), listener.close())
        .await
        .expect("listener close did not cancel stalled auth")
        .unwrap();
    drop(stalled);
    drop(valid);
    tokio::fs::remove_dir_all(directory).await.unwrap();
}
