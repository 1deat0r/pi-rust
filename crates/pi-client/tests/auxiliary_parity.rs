use pi_client::transport::{ByteTransport, TransportFactory, TransportFuture, TransportHandlers};
use pi_client::{
    AcquireSessionOptions, ClientConnectionState, PiClient, PiClientError, ReconnectBackoff,
    SessionLeaseMode,
};
use pi_protocol::{
    ClientMessage, ClientMessageDecoder, Command, CommandResult, ModelRef, ServerEvent,
    ServerMessage, ServerSnapshot, SessionPhase, SessionSnapshot, ThinkingLevel,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

#[derive(Clone)]
struct FakeFactory {
    attempts: Arc<AtomicUsize>,
    failures: Arc<Mutex<VecDeque<String>>>,
    connections: Arc<Mutex<Vec<FakeConnection>>>,
    on_message: Arc<dyn Fn(usize, ClientMessage, TransportHandlers) + Send + Sync>,
}

#[derive(Clone)]
struct FakeConnection {
    index: usize,
    handlers: TransportHandlers,
    sent: Arc<Mutex<Vec<ClientMessage>>>,
    closed: Arc<AtomicBool>,
    on_message: Arc<dyn Fn(usize, ClientMessage, TransportHandlers) + Send + Sync>,
}

struct FakeTransport {
    connection: FakeConnection,
    decoder: Mutex<ClientMessageDecoder>,
}

struct EarlyDataFactory {
    close_count: Arc<AtomicUsize>,
}

struct EarlyDataTransport {
    close_count: Arc<AtomicUsize>,
}

impl FakeFactory {
    fn new(
        failures: impl IntoIterator<Item = String>,
        on_message: impl Fn(usize, ClientMessage, TransportHandlers) + Send + Sync + 'static,
    ) -> Self {
        Self {
            attempts: Arc::new(AtomicUsize::new(0)),
            failures: Arc::new(Mutex::new(failures.into_iter().collect())),
            connections: Arc::new(Mutex::new(Vec::new())),
            on_message: Arc::new(on_message),
        }
    }

    fn connection(&self, index: usize) -> FakeConnection {
        self.connections.lock().unwrap()[index].clone()
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl TransportFactory for FakeFactory {
    fn connect<'a>(
        &'a self,
        handlers: TransportHandlers,
    ) -> TransportFuture<'a, Result<Arc<dyn ByteTransport>, PiClientError>> {
        Box::pin(async move {
            let index = self.attempts.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.failures.lock().unwrap().pop_front() {
                return Err(PiClientError { message: error });
            }
            let connection = FakeConnection {
                index,
                handlers,
                sent: Arc::new(Mutex::new(Vec::new())),
                closed: Arc::new(AtomicBool::new(false)),
                on_message: self.on_message.clone(),
            };
            self.connections.lock().unwrap().push(connection.clone());
            let transport: Arc<dyn ByteTransport> = Arc::new(FakeTransport {
                connection,
                decoder: Mutex::new(ClientMessageDecoder::new(&Default::default()).unwrap()),
            });
            Ok(transport)
        })
    }
}

impl TransportFactory for EarlyDataFactory {
    fn connect<'a>(
        &'a self,
        handlers: TransportHandlers,
    ) -> TransportFuture<'a, Result<Arc<dyn ByteTransport>, PiClientError>> {
        Box::pin(async move {
            send_server_message(
                &handlers,
                ServerMessage::Hello {
                    version: pi_protocol::PROTOCOL_VERSION,
                    connection_id: "early".into(),
                    snapshot: base_snapshot(1),
                },
            );
            Ok(Arc::new(EarlyDataTransport {
                close_count: self.close_count.clone(),
            }) as Arc<dyn ByteTransport>)
        })
    }
}

impl ByteTransport for EarlyDataTransport {
    fn send<'a>(&'a self, _chunk: Vec<u8>) -> TransportFuture<'a, Result<(), PiClientError>> {
        Box::pin(async {
            Err(PiClientError {
                message: "hello was not sent".into(),
            })
        })
    }

    fn close(&self) {
        self.close_count.fetch_add(1, Ordering::SeqCst);
    }
}

impl ByteTransport for FakeTransport {
    fn send<'a>(&'a self, chunk: Vec<u8>) -> TransportFuture<'a, Result<(), PiClientError>> {
        Box::pin(async move {
            if self.connection.closed.load(Ordering::SeqCst) {
                return Err(PiClientError {
                    message: "fake transport is closed".into(),
                });
            }
            let messages =
                self.decoder
                    .lock()
                    .unwrap()
                    .push(&chunk)
                    .map_err(|error| PiClientError {
                        message: error.to_string(),
                    })?;
            for message in messages {
                self.connection.sent.lock().unwrap().push(message.clone());
                (self.connection.on_message)(
                    self.connection.index,
                    message,
                    self.connection.handlers.clone(),
                );
            }
            Ok(())
        })
    }

    fn close(&self) {
        self.connection.closed.store(true, Ordering::SeqCst);
    }
}

fn send_server_message(handlers: &TransportHandlers, message: ServerMessage) {
    let frame = pi_protocol::encode_server_message(&message, &Default::default()).unwrap();
    (handlers.on_data)(frame);
}

fn base_snapshot(revision: i64) -> ServerSnapshot {
    ServerSnapshot {
        server_id: "fake-server".into(),
        protocol_version: pi_protocol::PROTOCOL_VERSION,
        revision,
        sessions: Vec::new(),
        models: Vec::new(),
    }
}

fn session_snapshot(id: &str, revision: i64, attached: bool) -> SessionSnapshot {
    SessionSnapshot {
        id: id.into(),
        name: None,
        cwd: "/workspace".into(),
        created_at: 1,
        updated_at: 1,
        phase: SessionPhase::Idle,
        model: ModelRef {
            provider: "faux".into(),
            id: "model".into(),
        },
        thinking_level: ThinkingLevel::Off,
        attached,
        locked: true,
        revision,
        transcript: Vec::new(),
        queued_steer: Vec::new(),
        queued_steer_count: 0,
    }
}

fn hello(index: usize, handlers: &TransportHandlers, revision: i64) {
    send_server_message(
        handlers,
        ServerMessage::Hello {
            version: pi_protocol::PROTOCOL_VERSION,
            connection_id: format!("connection-{index}"),
            snapshot: base_snapshot(revision),
        },
    );
}

fn client(factory: &FakeFactory, timeout: Duration) -> PiClient {
    PiClient::with_transport_factory(Arc::new(factory.clone()), timeout, timeout)
}

async fn wait_for<T>(mut read: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(value) = read() {
                return value;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture did not reach the expected state")
}

#[tokio::test]
async fn in_flight_requests_fail_and_are_not_replayed_after_reconnect() {
    let factory = FakeFactory::new(Vec::<String>::new(), |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            hello(index, &handlers, index as i64 + 1);
        }
    });
    let client = client(&factory, Duration::from_millis(100));
    let states = Arc::new(Mutex::new(Vec::new()));
    let states_for_listener = states.clone();
    let _unsubscribe = client.subscribe_connection_state(move |change| {
        states_for_listener.lock().unwrap().push(change.state);
    });

    client.reconnect().await.unwrap();
    let request_client = client.clone();
    let pending = tokio::spawn(async move { request_client.request(Command::List).await });
    wait_for(|| {
        let connection = factory.connection(0);
        let ready = connection.sent.lock().unwrap().len() >= 2;
        ready.then_some(())
    })
    .await;
    factory.connection(0).remote_close();

    let error = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.message.contains("closed"));
    assert_eq!(
        client.connection_state(),
        ClientConnectionState::Disconnected
    );

    client.reconnect().await.unwrap();
    let first_sent = factory.connection(0).sent.lock().unwrap().clone();
    let second_sent = factory.connection(1).sent.lock().unwrap().clone();
    assert!(matches!(first_sent[0], ClientMessage::Hello { .. }));
    assert!(matches!(first_sent[1], ClientMessage::Request { .. }));
    assert_eq!(
        second_sent.len(),
        1,
        "reconnect must not replay the request"
    );
    assert!(matches!(second_sent[0], ClientMessage::Hello { .. }));
    assert_eq!(
        states.lock().unwrap().as_slice(),
        &[
            ClientConnectionState::Connecting,
            ClientConnectionState::Connected,
            ClientConnectionState::Disconnected,
            ClientConnectionState::Connecting,
            ClientConnectionState::Connected,
        ]
    );
}

#[tokio::test]
async fn reconnect_backoff_retries_factory_failures_with_deterministic_delays() {
    let factory = FakeFactory::new(
        vec!["first failure".into(), "second failure".into()],
        |index, message, handlers| {
            if matches!(message, ClientMessage::Hello { .. }) {
                hello(index, &handlers, 3);
            }
        },
    );
    let client = client(&factory, Duration::from_millis(100));

    let snapshot = client
        .reconnect_with_backoff(ReconnectBackoff {
            max_attempts: 3,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        })
        .await
        .unwrap();
    assert_eq!(snapshot.revision, 3);
    assert_eq!(factory.attempts(), 3);
    assert_eq!(client.connection_state(), ClientConnectionState::Connected);
}

#[tokio::test]
async fn rejects_server_data_delivered_before_client_hello() {
    let close_count = Arc::new(AtomicUsize::new(0));
    let client = PiClient::with_transport_factory(
        Arc::new(EarlyDataFactory {
            close_count: close_count.clone(),
        }),
        Duration::from_millis(100),
        Duration::from_millis(100),
    );

    let error = client.reconnect().await.unwrap_err();
    assert_eq!(
        error.message,
        "Received server data before the client hello was sent"
    );
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        client.connection_state(),
        ClientConnectionState::Disconnected
    );
}

#[tokio::test]
async fn rejects_a_non_handshake_message_before_server_hello() {
    let factory = FakeFactory::new(Vec::<String>::new(), |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            send_server_message(
                &handlers,
                ServerMessage::Event {
                    event: ServerEvent::ServerSnapshot {
                        snapshot: base_snapshot(index as i64 + 1),
                    },
                },
            );
        }
    });
    let client = client(&factory, Duration::from_millis(100));

    let error = client.reconnect().await.unwrap_err();
    assert_eq!(error.message, "Expected server hello as first message");
    assert_eq!(
        client.connection_state(),
        ClientConnectionState::Disconnected
    );
}

#[tokio::test]
async fn leases_share_one_attachment_and_enforce_exclusive_mode() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_handler = requests.clone();
    let factory = FakeFactory::new(Vec::<String>::new(), move |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            hello(index, &handlers, 1);
            return;
        }
        let ClientMessage::Request { id, request } = message else {
            return;
        };
        requests_for_handler.lock().unwrap().push(request.clone());
        let response = match request {
            Command::Attach { session_id } => ServerMessage::Response {
                id,
                ok: true,
                result: Some(CommandResult::Attach {
                    session: session_snapshot(&session_id, 1, true),
                }),
                error: None,
            },
            Command::Detach { session_id } => ServerMessage::Response {
                id,
                ok: true,
                result: Some(CommandResult::Detach { session_id }),
                error: None,
            },
            _ => return,
        };
        send_server_message(&handlers, response);
    });
    let client = client(&factory, Duration::from_millis(100));
    client.reconnect().await.unwrap();

    let first = client.attach_session("session-1").await.unwrap();
    let second = client.attach_session("session-1").await.unwrap();
    assert!(first.attached());
    assert!(second.attached());
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|command| matches!(command, Command::Attach { .. }))
            .count(),
        1
    );

    first.detach().await.unwrap();
    assert!(second.attached());
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|command| matches!(command, Command::Detach { .. }))
            .count(),
        0
    );
    second.detach().await.unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|command| matches!(command, Command::Detach { .. }))
            .count(),
        1
    );

    let exclusive = client
        .acquire_session(
            "session-1",
            AcquireSessionOptions {
                mode: SessionLeaseMode::Exclusive,
            },
        )
        .await
        .unwrap();
    let conflict = client.attach_session("session-1").await.err().unwrap();
    assert!(conflict.message.contains("exclusive"));
    exclusive.dispose().await.unwrap();

    let disconnected = client.attach_session("session-1").await.unwrap();
    let detach_count = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|command| matches!(command, Command::Detach { .. }))
        .count();
    factory.connection(0).remote_close();
    assert!(!disconnected.active());
    disconnected.dispose().await.unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|command| matches!(command, Command::Detach { .. }))
            .count(),
        detach_count,
        "invalidated leases must not issue protocol cleanup"
    );
}

#[tokio::test]
async fn dispose_failure_is_reconciled_before_the_next_attach() {
    let detach_count = Arc::new(AtomicUsize::new(0));
    let detach_count_for_handler = detach_count.clone();
    let factory = FakeFactory::new(Vec::<String>::new(), move |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            hello(index, &handlers, 1);
            return;
        }
        let ClientMessage::Request { id, request } = message else {
            return;
        };
        match request {
            Command::Attach { session_id } => send_server_message(
                &handlers,
                ServerMessage::Response {
                    id,
                    ok: true,
                    result: Some(CommandResult::Attach {
                        session: session_snapshot(&session_id, 1, true),
                    }),
                    error: None,
                },
            ),
            Command::Detach { session_id } => {
                let count = detach_count_for_handler.fetch_add(1, Ordering::SeqCst);
                send_server_message(
                    &handlers,
                    ServerMessage::Response {
                        id,
                        ok: count > 0,
                        result: (count > 0).then_some(CommandResult::Detach {
                            session_id: session_id.clone(),
                        }),
                        error: (count == 0).then_some(pi_protocol::ProtocolError {
                            code: pi_protocol::ProtocolErrorCode::InvalidRequest,
                            message: "detach failed".into(),
                            details: None,
                        }),
                    },
                );
            }
            _ => {}
        }
    });
    let client = client(&factory, Duration::from_millis(100));
    client.reconnect().await.unwrap();

    let failed = client
        .acquire_session(
            "session-1",
            AcquireSessionOptions {
                mode: SessionLeaseMode::Exclusive,
            },
        )
        .await
        .unwrap();
    assert!(failed.dispose().await.is_err());
    assert!(!failed.attached(), "dispose relinquishes a failed lease");

    let reacquired = client.attach_session("session-1").await.unwrap();
    assert!(reacquired.attached());
    assert_eq!(detach_count.load(Ordering::SeqCst), 2);
    reacquired.dispose().await.unwrap();
}

#[tokio::test]
async fn snapshot_reconciliation_keeps_newer_events_and_allows_lower_revisions_after_detach() {
    let pending_attach = Arc::new(Mutex::new(None));
    let pending_attach_for_handler = pending_attach.clone();
    let factory = FakeFactory::new(Vec::<String>::new(), move |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            hello(index, &handlers, 1);
            return;
        }
        let ClientMessage::Request { id, request } = message else {
            return;
        };
        match request {
            Command::Attach { session_id } => {
                if pending_attach_for_handler.lock().unwrap().is_none() {
                    *pending_attach_for_handler.lock().unwrap() = Some((id, handlers, session_id));
                }
            }
            Command::Detach { session_id } => send_server_message(
                &handlers,
                ServerMessage::Response {
                    id,
                    ok: true,
                    result: Some(CommandResult::Detach { session_id }),
                    error: None,
                },
            ),
            _ => {}
        }
    });
    let client = client(&factory, Duration::from_millis(100));
    client.reconnect().await.unwrap();

    let attach_client = client.clone();
    let attaching = tokio::spawn(async move { attach_client.attach_session("session-1").await });
    wait_for(|| pending_attach.lock().unwrap().as_ref().map(|_| ())).await;
    let (id, handlers, session_id) = pending_attach.lock().unwrap().take().unwrap();
    send_server_message(
        &handlers,
        ServerMessage::Event {
            event: ServerEvent::SessionSnapshot {
                snapshot: session_snapshot(&session_id, 3, true),
            },
        },
    );
    send_server_message(
        &handlers,
        ServerMessage::Response {
            id,
            ok: true,
            result: Some(CommandResult::Attach {
                session: session_snapshot(&session_id, 2, true),
            }),
            error: None,
        },
    );
    let handle = attaching.await.unwrap().unwrap();
    assert_eq!(handle.snapshot().unwrap().revision, 3);

    handle.detach().await.unwrap();
    let attach_client = client.clone();
    let reopened = tokio::spawn(async move { attach_client.attach_session("session-1").await });
    wait_for(|| pending_attach.lock().unwrap().as_ref().map(|_| ())).await;
    let (id, handlers, session_id) = pending_attach.lock().unwrap().take().unwrap();
    send_server_message(
        &handlers,
        ServerMessage::Response {
            id,
            ok: true,
            result: Some(CommandResult::Attach {
                session: session_snapshot(&session_id, 0, true),
            }),
            error: None,
        },
    );
    assert_eq!(
        reopened
            .await
            .unwrap()
            .unwrap()
            .snapshot()
            .unwrap()
            .revision,
        0
    );
}

#[tokio::test]
async fn request_timeout_suppresses_late_response_and_dispose_is_terminal() {
    let factory = FakeFactory::new(Vec::<String>::new(), |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            hello(index, &handlers, 1);
        }
    });
    let client = client(&factory, Duration::from_millis(20));
    client.reconnect().await.unwrap();
    let request_client = client.clone();
    let pending = tokio::spawn(async move { request_client.request(Command::List).await });
    wait_for(|| {
        let connection = factory.connection(0);
        let ready = connection.sent.lock().unwrap().len() >= 2;
        ready.then_some(())
    })
    .await;
    let error = pending.await.unwrap().unwrap_err();
    assert!(error.message.contains("timed out"));

    let request_id = match factory.connection(0).sent.lock().unwrap()[1].clone() {
        ClientMessage::Request { id, .. } => id,
        _ => panic!("expected list request"),
    };
    send_server_message(
        &factory.connection(0).handlers,
        ServerMessage::Response {
            id: request_id,
            ok: true,
            result: Some(CommandResult::List { sessions: vec![] }),
            error: None,
        },
    );
    assert_eq!(client.connection_state(), ClientConnectionState::Connected);

    let request_client = client.clone();
    let pending = tokio::spawn(async move { request_client.request(Command::List).await });
    wait_for(|| (factory.connection(0).sent.lock().unwrap().len() >= 3).then_some(())).await;
    let client_for_dispose = client.clone();
    let disposal = tokio::spawn(async move { client_for_dispose.dispose().await });
    let disposed_error = pending.await.unwrap().unwrap_err();
    disposal.await.unwrap().unwrap();
    assert!(disposed_error.message.contains("disposed"));
    assert!(client.is_disposed());
    assert_eq!(
        client.connection_state(),
        ClientConnectionState::Disconnected
    );
    client.dispose().await.unwrap();
    assert!(client
        .reconnect()
        .await
        .unwrap_err()
        .message
        .contains("disposed"));
}

#[cfg(unix)]
#[tokio::test]
async fn unix_transport_factory_exchanges_fragmented_frames() {
    let path =
        std::env::temp_dir().join(format!("pi-client-factory-{}.sock", uuid::Uuid::new_v4()));
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let mut decoder = ClientMessageDecoder::new(&Default::default()).unwrap();
        let mut buffer = [0u8; 1024];
        loop {
            let count = reader.read(&mut buffer).await.unwrap();
            for message in decoder.push(&buffer[..count]).unwrap() {
                match message {
                    ClientMessage::Hello { .. } => {
                        let frame = pi_protocol::encode_server_message(
                            &ServerMessage::Hello {
                                version: pi_protocol::PROTOCOL_VERSION,
                                connection_id: "unix".into(),
                                snapshot: base_snapshot(7),
                            },
                            &Default::default(),
                        )
                        .unwrap();
                        let split = 3.min(frame.len());
                        writer.write_all(&frame[..split]).await.unwrap();
                        writer.write_all(&frame[split..]).await.unwrap();
                    }
                    ClientMessage::Request {
                        id,
                        request: Command::List,
                    } => {
                        let frame = pi_protocol::encode_server_message(
                            &ServerMessage::Response {
                                id,
                                ok: true,
                                result: Some(CommandResult::List { sessions: vec![] }),
                                error: None,
                            },
                            &Default::default(),
                        )
                        .unwrap();
                        writer.write_all(&frame).await.unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        }
    });

    let client = PiClient::connect(path.to_str().unwrap()).await.unwrap();
    assert_eq!(client.snapshot().unwrap().revision, 7);
    assert!(matches!(
        client.request(Command::List).await.unwrap(),
        CommandResult::List { .. }
    ));
    client.dispose().await.unwrap();
    server.await.unwrap();
    let _ = std::fs::remove_file(path);
}

impl FakeConnection {
    fn remote_close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            (self.handlers.on_close)();
        }
    }
}

impl FakeTransport {
    #[allow(dead_code)]
    fn index(&self) -> usize {
        self.connection.index
    }
}

#[tokio::test]
async fn event_subscription_can_be_removed_without_affecting_connection() {
    let factory = FakeFactory::new(Vec::<String>::new(), |index, message, handlers| {
        if matches!(message, ClientMessage::Hello { .. }) {
            hello(index, &handlers, 1);
        }
    });
    let client = client(&factory, Duration::from_millis(100));
    client.reconnect().await.unwrap();

    let events = Arc::new(AtomicUsize::new(0));
    let events_for_listener = events.clone();
    let unsubscribe = client.subscribe(Arc::new(move |_| {
        events_for_listener.fetch_add(1, Ordering::SeqCst);
    }));
    send_server_message(
        &factory.connection(0).handlers,
        ServerMessage::Event {
            event: ServerEvent::ServerSnapshot {
                snapshot: base_snapshot(2),
            },
        },
    );
    assert_eq!(events.load(Ordering::SeqCst), 1);

    unsubscribe.unsubscribe();
    send_server_message(
        &factory.connection(0).handlers,
        ServerMessage::Event {
            event: ServerEvent::ServerSnapshot {
                snapshot: base_snapshot(3),
            },
        },
    );
    assert_eq!(events.load(Ordering::SeqCst), 1);
    assert_eq!(client.connection_state(), ClientConnectionState::Connected);
    client.dispose().await.unwrap();
}
