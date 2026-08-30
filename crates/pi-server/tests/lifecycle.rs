#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pi_server::{
    ByteConnectionAcceptor, PiServer, PiServerListener, PiServerOptions, TestServerService,
    UnixListener,
};

struct FixtureListener {
    starts: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    fail: bool,
}

impl PiServerListener for FixtureListener {
    fn address(&self) -> Option<String> {
        None
    }

    fn start(
        &mut self,
        _accept: ByteConnectionAcceptor,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err("fixture listener failed".to_string())
            } else {
                Ok(())
            }
        })
    }

    fn close(&mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn start_is_single_use_even_when_the_server_has_no_live_clients() {
    let directory = std::env::temp_dir().join(format!("pi-server-start-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let socket = directory.join("server.sock").to_string_lossy().into_owned();
    let mut server = PiServer::new(
        Box::new(TestServerService::new()),
        PiServerOptions {
            listeners: vec![Box::new(UnixListener::new(socket).unwrap())],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("server-1".to_string()),
            on_error: None,
        },
    )
    .unwrap();

    server.start().await.unwrap();
    let second_start = server.start().await.unwrap_err();
    assert!(second_start.contains("already started"));
    server.close().await.unwrap();
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn start_failure_rolls_back_prior_listeners_and_closes_the_server() {
    let starts = Arc::new(AtomicUsize::new(0));
    let first_closes = Arc::new(AtomicUsize::new(0));
    let second_closes = Arc::new(AtomicUsize::new(0));
    let mut server = PiServer::new(
        Box::new(TestServerService::new()),
        PiServerOptions {
            listeners: vec![
                Box::new(FixtureListener {
                    starts: starts.clone(),
                    closes: first_closes.clone(),
                    fail: false,
                }),
                Box::new(FixtureListener {
                    starts: starts.clone(),
                    closes: second_closes.clone(),
                    fail: true,
                }),
            ],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("server-1".to_string()),
            on_error: None,
        },
    )
    .unwrap();

    assert_eq!(server.start().await.unwrap_err(), "fixture listener failed");
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(first_closes.load(Ordering::SeqCst), 1);
    assert_eq!(second_closes.load(Ordering::SeqCst), 0);
    assert!(server.start().await.unwrap_err().contains("closing"));
    server.close().await.unwrap();
}
