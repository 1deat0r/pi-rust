#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Live reconnect and lease-churn coverage for the pi-server/pi-client pair.
//!
//! The server and client contracts are deliberately exercised through the
//! public Unix transport APIs.  The small scripted transport wrapper only
//! supplies deterministic connect failures and one dropped response; every
//! successful connection and every protocol message still crosses a real
//! local Unix socket.

use std::collections::HashSet;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_client::transport::{
    ByteTransport, TransportFactory, TransportFuture, TransportHandlers, UnixTransportFactory,
};
use pi_client::{
    AcquireSessionOptions, ClientConnectionState, PiClient, PiClientError, ReconnectBackoff,
    SessionLeaseMode,
};
use pi_protocol::{
    encode_client_message, encode_frame, encode_server_message, ClientMessage,
    ClientMessageDecoder, Command, CommandResult, ProtocolErrorCode, ServerMessage,
    ServerMessageDecoder, PROTOCOL_VERSION,
};
use pi_server::types::PiServerOptions;
use pi_server::{InMemoryService, PiServer, UnixListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn test_models() -> Vec<pi_protocol::ModelMetadata> {
    vec![pi_protocol::ModelMetadata {
        provider: "faux".into(),
        id: "faux-1".into(),
        name: "Faux Model".into(),
        api: "faux".into(),
        reasoning: false,
        input: vec![
            pi_protocol::ModelInput::Text,
            pi_protocol::ModelInput::Image,
        ],
        context_window: 128_000,
        max_tokens: 16_384,
        cost: pi_protocol::ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        supported_thinking_levels: vec![pi_protocol::ThinkingLevel::Off],
        authenticated: false,
    }]
}

fn scripted_transport(path: impl Into<PathBuf>) -> ScriptedUnixFactory {
    ScriptedUnixFactory::new(path)
}

#[derive(Clone)]
struct ScriptedUnixFactory {
    inner: UnixTransportFactory,
    attempts: Arc<AtomicUsize>,
    forced_connect_failures: Arc<AtomicUsize>,
    drop_next_detach_response: Arc<AtomicBool>,
    commands: Arc<Mutex<Vec<Command>>>,
}

impl ScriptedUnixFactory {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: UnixTransportFactory::new(path).expect("valid Unix socket path"),
            attempts: Arc::new(AtomicUsize::new(0)),
            forced_connect_failures: Arc::new(AtomicUsize::new(0)),
            drop_next_detach_response: Arc::new(AtomicBool::new(false)),
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set_forced_connect_failures(&self, failures: usize) {
        self.forced_connect_failures
            .store(failures, Ordering::SeqCst);
    }

    fn arm_detach_response_drop(&self) {
        self.drop_next_detach_response.store(true, Ordering::SeqCst);
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    fn commands(&self) -> Vec<Command> {
        self.commands.lock().unwrap().clone()
    }

    fn take_connect_failure(&self) -> bool {
        loop {
            let remaining = self.forced_connect_failures.load(Ordering::SeqCst);
            if remaining == 0 {
                return false;
            }
            if self
                .forced_connect_failures
                .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }
}

struct ScriptedConnectionState {
    dropped_response_ids: Arc<Mutex<HashSet<String>>>,
}

struct ScriptedTransport {
    inner: Arc<dyn ByteTransport>,
    outgoing: Mutex<ClientMessageDecoder>,
    state: ScriptedConnectionState,
    drop_next_detach_response: Arc<AtomicBool>,
    commands: Arc<Mutex<Vec<Command>>>,
}

impl TransportFactory for ScriptedUnixFactory {
    fn connect<'a>(
        &'a self,
        handlers: TransportHandlers,
    ) -> TransportFuture<'a, Result<Arc<dyn ByteTransport>, PiClientError>> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.take_connect_failure() {
            return Box::pin(async move {
                Err(PiClientError {
                    message: format!("deterministic connect failure #{attempt}"),
                })
            });
        }

        let dropped_response_ids = Arc::new(Mutex::new(HashSet::new()));
        let inbound_decoder = Arc::new(Mutex::new(
            ServerMessageDecoder::new(&Default::default()).expect("server decoder options"),
        ));
        let data_handlers = handlers.clone();
        let close_handlers = handlers.clone();
        let error_handlers = handlers;
        let inbound_decoder_for_handler = inbound_decoder.clone();
        let dropped_response_ids_for_handler = dropped_response_ids.clone();
        let wrapped_handlers = TransportHandlers::new(
            move |chunk| {
                let messages = match inbound_decoder_for_handler.lock().unwrap().push(&chunk) {
                    Ok(messages) => messages,
                    Err(error) => {
                        (data_handlers.on_error)(PiClientError {
                            message: format!("scripted inbound decode: {error}"),
                        });
                        return;
                    }
                };
                for message in messages {
                    let drop_message = match &message {
                        ServerMessage::Response { id, .. } => {
                            dropped_response_ids_for_handler.lock().unwrap().remove(id)
                        }
                        _ => false,
                    };
                    if drop_message {
                        continue;
                    }
                    match encode_server_message(&message, &Default::default()) {
                        Ok(frame) => (data_handlers.on_data)(frame),
                        Err(error) => (data_handlers.on_error)(PiClientError {
                            message: format!("scripted inbound encode: {error}"),
                        }),
                    }
                }
            },
            move || (close_handlers.on_close)(),
            move |error| (error_handlers.on_error)(error),
        );

        let inner = &self.inner;
        let drop_next_detach_response = self.drop_next_detach_response.clone();
        let commands = self.commands.clone();
        Box::pin(async move {
            let transport = inner.connect(wrapped_handlers).await?;
            let transport: Arc<dyn ByteTransport> = Arc::new(ScriptedTransport {
                inner: transport,
                outgoing: Mutex::new(
                    ClientMessageDecoder::new(&Default::default()).expect("client decoder options"),
                ),
                state: ScriptedConnectionState {
                    dropped_response_ids,
                },
                drop_next_detach_response,
                commands,
            });
            Ok(transport)
        })
    }
}

impl ByteTransport for ScriptedTransport {
    fn send<'a>(&'a self, chunk: Vec<u8>) -> TransportFuture<'a, Result<(), PiClientError>> {
        Box::pin(async move {
            let messages = self
                .outgoing
                .lock()
                .unwrap()
                .push(&chunk)
                .map_err(|error| PiClientError {
                    message: format!("scripted outbound decode: {error}"),
                })?;
            for message in messages {
                let ClientMessage::Request { id, request } = message else {
                    continue;
                };
                let drop_response = matches!(request, Command::Detach { .. })
                    && self.drop_next_detach_response.swap(false, Ordering::SeqCst);
                self.commands.lock().unwrap().push(request);
                if drop_response {
                    self.state.dropped_response_ids.lock().unwrap().insert(id);
                }
            }
            self.inner.send(chunk).await
        })
    }

    fn close(&self) {
        self.inner.close();
    }
}

fn server_options(socket_path: &Path, server_id: &str) -> PiServerOptions {
    PiServerOptions {
        listeners: vec![Box::new(
            UnixListener::new(socket_path.to_string_lossy().into_owned())
                .expect("valid Unix listener path"),
        )],
        max_frame_length: None,
        handshake_timeout_ms: None,
        server_id: Some(server_id.to_string()),
        on_error: None,
    }
}

async fn start_server(socket_path: &Path, service: &InMemoryService, server_id: &str) -> PiServer {
    let mut server = PiServer::new(
        Box::new(service.clone()),
        server_options(socket_path, server_id),
    )
    .expect("construct PiServer");
    server.start().await.expect("start PiServer");
    server
}

async fn test_directory(label: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("pi-server-d3-{label}-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("create test directory");
    directory
}

async fn cleanup(directory: &Path) {
    tokio::fs::remove_dir_all(directory)
        .await
        .expect("remove test directory");
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture did not reach the expected lifecycle state");
}

fn command_kind(command: &Command) -> &'static str {
    match command {
        Command::List => "list",
        Command::Create { .. } => "create",
        Command::Attach { .. } => "attach",
        Command::Detach { .. } => "detach",
        Command::Prompt { .. } => "prompt",
        Command::Steer { .. } => "steer",
        Command::Abort { .. } => "abort",
        Command::SetModel { .. } => "set_model",
        Command::SetThinking { .. } => "set_thinking",
    }
}

async fn read_server_message(stream: &mut UnixStream) -> IoResult<ServerMessage> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    let mut frame = vec![0u8; 4 + length];
    frame[..4].copy_from_slice(&header);
    stream.read_exact(&mut frame[4..]).await?;
    let mut decoder = ServerMessageDecoder::new(&Default::default())
        .map_err(|error| Error::new(ErrorKind::InvalidData, error.to_string()))?;
    decoder
        .push(&frame)
        .map_err(|error| Error::new(ErrorKind::InvalidData, error.to_string()))?
        .into_iter()
        .next()
        .ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "server sent an empty frame"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_backoff_reaches_a_restarted_server_over_unix_socket() {
    let directory = test_directory("reconnect").await;
    let socket_path = directory.join("server.sock");
    let service = InMemoryService::new(test_models());
    let mut server = start_server(&socket_path, &service, "server-before-restart").await;
    let factory = scripted_transport(&socket_path);
    let client = PiClient::with_transport_factory(
        Arc::new(factory.clone()),
        Duration::from_millis(100),
        Duration::from_millis(100),
    );
    let states = Arc::new(Mutex::new(Vec::new()));
    let states_for_listener = states.clone();
    let _unsubscribe = client.subscribe_connection_state(move |change| {
        states_for_listener.lock().unwrap().push(change.state);
    });

    client.reconnect().await.expect("initial Unix handshake");
    let created = client
        .request(Command::Create {
            cwd: Some(directory.to_string_lossy().into_owned()),
            name: Some("reconnect session".into()),
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create session");
    let session_id = match created {
        CommandResult::Create { session } => session.id,
        other => panic!("expected create result, got {other:?}"),
    };

    client.close().await.expect("disconnect client");
    wait_until(|| client.connection_state() == ClientConnectionState::Disconnected).await;
    server.close().await.expect("close first server");

    factory.set_forced_connect_failures(2);
    server = start_server(&socket_path, &service, "server-after-restart").await;
    let attempts_before_restart = factory.attempts();
    let snapshot = client
        .reconnect_with_backoff(ReconnectBackoff {
            max_attempts: 3,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        })
        .await
        .expect("bounded reconnect should reach the restarted server");

    assert_eq!(snapshot.server_id, "server-after-restart");
    assert_eq!(factory.attempts() - attempts_before_restart, 3);
    assert!(
        states
            .lock()
            .unwrap()
            .iter()
            .filter(|state| **state == ClientConnectionState::Connecting)
            .count()
            >= 4
    );
    let listed = client
        .request(Command::List)
        .await
        .expect("list after reconnect");
    match listed {
        CommandResult::List { sessions } => {
            assert!(sessions.iter().any(|session| session.id == session_id));
        }
        other => panic!("expected list result, got {other:?}"),
    }

    client.dispose().await.expect("terminal client disposal");
    server.close().await.expect("close restarted server");
    cleanup(&directory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_churn_reconciles_a_timed_out_detach_and_preserves_exclusive_attach() {
    let directory = test_directory("lease").await;
    let socket_path = directory.join("server.sock");
    let service = InMemoryService::new(test_models());
    let mut server = start_server(&socket_path, &service, "lease-server").await;
    let factory = scripted_transport(&socket_path);
    let client = PiClient::with_transport_factory(
        Arc::new(factory.clone()),
        Duration::from_millis(100),
        Duration::from_millis(20),
    );
    client.reconnect().await.expect("Unix handshake");

    let created = client
        .request(Command::Create {
            cwd: Some(directory.to_string_lossy().into_owned()),
            name: Some("lease churn".into()),
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create session for lease churn");
    let session_id = match created {
        CommandResult::Create { session } => session.id,
        other => panic!("expected create result, got {other:?}"),
    };
    let first = client
        .acquire_session(
            &session_id,
            AcquireSessionOptions {
                mode: SessionLeaseMode::Shared,
            },
        )
        .await
        .expect("acquire first shared lease");
    let second = client
        .attach_session(&session_id)
        .await
        .expect("second shared lease");
    assert!(first.attached());
    assert!(second.attached());

    let exclusive_conflict = match client
        .acquire_session(
            &session_id,
            AcquireSessionOptions {
                mode: SessionLeaseMode::Exclusive,
            },
        )
        .await
    {
        Ok(_) => panic!("exclusive attach must conflict with shared leases"),
        Err(error) => error,
    };
    assert!(exclusive_conflict.message.contains("active lease"));

    first.detach().await.expect("release first shared lease");
    assert!(second.attached());

    // Keep the real socket and server response path, but drop exactly this
    // detach response so SessionHandle::dispose exercises its timeout and
    // cleanup-required reconciliation branch deterministically.
    factory.arm_detach_response_drop();
    let timeout_error = second
        .dispose()
        .await
        .expect_err("dropped detach response must time out");
    assert!(timeout_error.message.contains("timed out"));
    assert!(!second.active());

    let exclusive = client
        .acquire_session(
            &session_id,
            AcquireSessionOptions {
                mode: SessionLeaseMode::Exclusive,
            },
        )
        .await
        .expect("reconcile stale detach before exclusive attach");
    assert!(exclusive.active());
    exclusive.dispose().await.expect("release reconciled lease");

    let command_kinds: Vec<_> = factory.commands().iter().map(command_kind).collect();
    assert_eq!(
        command_kinds,
        vec!["create", "attach", "detach", "detach", "attach", "detach"]
    );

    client.dispose().await.expect("terminal client disposal");
    server.close().await.expect("close lease server");
    cleanup(&directory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_reattach_and_terminal_disposal_invalidate_session_handles() {
    let directory = test_directory("lifecycle").await;
    let socket_path = directory.join("server.sock");
    let service = InMemoryService::new(test_models());
    let mut server = start_server(&socket_path, &service, "lifecycle-server").await;
    let factory = scripted_transport(&socket_path);
    let client = PiClient::with_transport_factory(
        Arc::new(factory.clone()),
        Duration::from_millis(100),
        Duration::from_millis(100),
    );
    client.reconnect().await.expect("Unix handshake");

    let created = client
        .request(Command::Create {
            cwd: Some(directory.to_string_lossy().into_owned()),
            name: Some("lifecycle".into()),
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create lifecycle session");
    let session_id = match created {
        CommandResult::Create { session } => session.id,
        other => panic!("expected create result, got {other:?}"),
    };
    let handle = client
        .acquire_session(
            &session_id,
            AcquireSessionOptions {
                mode: SessionLeaseMode::Exclusive,
            },
        )
        .await
        .expect("acquire exclusive lifecycle lease");
    handle.prompt("survive disconnect").await.expect("prompt");

    client.close().await.expect("close client connection");
    wait_until(|| client.connection_state() == ClientConnectionState::Disconnected).await;
    assert!(!handle.active());
    let commands_before_invalidated_dispose = factory.commands().len();
    handle
        .dispose()
        .await
        .expect("invalidated handle disposal is a no-op");
    assert_eq!(
        factory.commands().len(),
        commands_before_invalidated_dispose
    );

    client.reconnect().await.expect("reattach handshake");
    let reattached = client
        .acquire_session(
            &session_id,
            AcquireSessionOptions {
                mode: SessionLeaseMode::Exclusive,
            },
        )
        .await
        .expect("reattach after disconnect");
    assert!(reattached.active());
    reattached.abort().await.expect("return session to idle");

    client.dispose().await.expect("permanent client disposal");
    assert!(client.is_disposed());
    assert!(!reattached.active());
    reattached
        .dispose()
        .await
        .expect("invalidated handle disposal after client dispose");
    assert!(client.reconnect().await.is_err());

    server.close().await.expect("session close removes socket");
    assert!(tokio::fs::symlink_metadata(&socket_path).await.is_err());
    cleanup(&directory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fragmented_and_malformed_unix_frames_follow_server_close_lifecycle() {
    let directory = test_directory("protocol").await;
    let socket_path = directory.join("server.sock");
    let service = InMemoryService::new(test_models());
    let mut server = start_server(&socket_path, &service, "protocol-server").await;

    let mut fragmented = UnixStream::connect(&socket_path)
        .await
        .expect("connect fragmented client");
    let hello = encode_client_message(
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        &Default::default(),
    )
    .expect("encode hello");
    fragmented
        .write_all(&hello[..1])
        .await
        .expect("write first hello fragment");
    fragmented
        .write_all(&hello[1..])
        .await
        .expect("write remaining hello fragment");
    fragmented.flush().await.expect("flush hello");
    let response =
        tokio::time::timeout(Duration::from_secs(1), read_server_message(&mut fragmented))
            .await
            .expect("fragmented hello response timeout")
            .expect("read fragmented hello response");
    assert!(
        matches!(response, ServerMessage::Hello { version, .. } if version == PROTOCOL_VERSION)
    );
    fragmented
        .shutdown()
        .await
        .expect("close fragmented client");

    let mut malformed = UnixStream::connect(&socket_path)
        .await
        .expect("connect malformed client");
    let malformed_frame = encode_frame(&[0xff]).expect("encode malformed frame");
    malformed
        .write_all(&malformed_frame)
        .await
        .expect("write malformed frame");
    malformed.flush().await.expect("flush malformed frame");
    let response =
        tokio::time::timeout(Duration::from_secs(1), read_server_message(&mut malformed))
            .await
            .expect("malformed response timeout")
            .expect("read malformed response");
    match response {
        ServerMessage::HelloError { error } => {
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        }
        other => panic!("expected malformed hello error, got {other:?}"),
    }
    malformed.shutdown().await.expect("close malformed client");

    server.close().await.expect("close protocol server");
    cleanup(&directory).await;
}
