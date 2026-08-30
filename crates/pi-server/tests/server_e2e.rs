#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! End-to-end protocol-link test: PiServer over a Unix socket, driven by
//! PiClient (hello handshake → list/create/prompt → snapshot events).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_protocol::{
    encode_client_message, encode_frame, ClientMessage, Command, CommandResult,
    FrameDecoderOptions, ProtocolErrorCode, ServerEvent, ServerMessage, ServerMessageDecoder,
    PROTOCOL_VERSION,
};
use pi_server::server::PiServer;
use pi_server::service::{
    Deferred, InMemoryService, PiServerService, PiSessionRuntime, TestServerService,
};
use pi_server::UnixListener;
use pi_server::{PiServerError, TestSessionRuntime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Test-side accessor: `latest_runtime` now returns `Option`; tests expect
/// the runtime to exist.
trait LatestRuntimeExpect {
    fn latest_runtime_expect(&self, id: &str) -> TestSessionRuntime;
}

impl LatestRuntimeExpect for TestServerService {
    fn latest_runtime_expect(&self, id: &str) -> TestSessionRuntime {
        self.latest_runtime(id).expect("runtime exists in test")
    }
}

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

#[tokio::test]
async fn client_server_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pi-server-e2e-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let socket_path = dir.join("pi.sock").to_string_lossy().into_owned();

    let service = Box::new(InMemoryService::new(test_models()));
    let mut server = PiServer::new(
        service,
        pi_server::types::PiServerOptions {
            listeners: vec![Box::new(UnixListener::new(socket_path.clone()).unwrap())],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("e2e-server".into()),
            on_error: None,
        },
    )
    .unwrap();
    server.start().await.unwrap();

    // Client connects + handshakes.
    let client = pi_client::PiClient::connect(&socket_path).await.unwrap();
    let snapshot = client.snapshot().unwrap();
    assert_eq!(snapshot.server_id, "e2e-server");
    assert_eq!(snapshot.sessions.len(), 0);
    assert_eq!(snapshot.models.len(), 1);

    // List sessions.
    let result = client.request(Command::List).await.unwrap();
    if let CommandResult::List { sessions } = result {
        assert!(sessions.is_empty());
    } else {
        panic!("expected List result");
    }

    // Create a session.
    let result = client
        .request(Command::Create {
            cwd: Some(dir.to_string_lossy().into_owned()),
            name: Some("e2e session".into()),
            model: None,
            thinking_level: None,
        })
        .await
        .unwrap();
    let session_id = match &result {
        CommandResult::Create { session } => session.id.clone(),
        _ => panic!("expected Create result"),
    };

    // Prompt the session; the snapshot must carry the user transcript item.
    let result = client
        .request(Command::Prompt {
            session_id: session_id.clone(),
            text: "hello server".into(),
        })
        .await
        .unwrap();
    match result {
        CommandResult::Prompt { session } => {
            assert_eq!(session.id, session_id);
            assert_eq!(session.transcript.len(), 1);
            match &session.transcript[0] {
                pi_protocol::TranscriptItem::User(u) => {
                    assert_eq!(u.role, "user");
                }
                _ => panic!("expected user transcript item"),
            }
        }
        _ => panic!("expected Prompt result"),
    }

    // List now shows the session.
    let result = client.request(Command::List).await.unwrap();
    if let CommandResult::List { sessions } = result {
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);
    } else {
        panic!("expected List result");
    }

    // Set model + thinking on the session.
    let result = client
        .request(Command::SetModel {
            session_id: session_id.clone(),
            model: pi_protocol::ModelRef {
                provider: "faux".into(),
                id: "faux-1".into(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(result, CommandResult::SetModel { .. }));

    let result = client
        .request(Command::SetThinking {
            session_id: session_id.clone(),
            thinking_level: pi_protocol::ThinkingLevel::Off,
        })
        .await
        .unwrap();
    assert!(matches!(result, CommandResult::SetThinking { .. }));

    // Abort is accepted.
    let result = client.request(Command::Abort { session_id }).await.unwrap();
    assert!(matches!(result, CommandResult::Abort { .. }));

    // Snapshot events: subscribe before a detach, then observe.
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let events = events.clone();
        client.subscribe(Arc::new(move |event| {
            if let pi_protocol::ServerEvent::ServerSnapshot { .. } = event {
                events.lock().unwrap().push("snapshot".to_string());
            }
        }));
    }
    let _ = client.request(Command::List).await.unwrap(); // triggers broadcast after response
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !events.lock().unwrap().is_empty(),
        "expected snapshot broadcast events"
    );

    let _ = client.close().await;
    drop(client);
    let _ = server.close().await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn bad_protocol_version_gets_hello_error() {
    let dir = std::env::temp_dir().join(format!("pi-server-ver-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let socket_path = dir.join("pi.sock").to_string_lossy().into_owned();

    let service = Box::new(InMemoryService::new(test_models()));
    let mut server = PiServer::new(
        service,
        pi_server::types::PiServerOptions {
            listeners: vec![Box::new(UnixListener::new(socket_path.clone()).unwrap())],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: Some("e2e-server".into()),
            on_error: None,
        },
    )
    .unwrap();
    server.start().await.unwrap();

    // Send a bad-version hello directly and expect hello_error.
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bad_hello = pi_protocol::ClientMessage::Hello { version: 999 };
    // encode_client_message returns a complete length-prefixed frame.
    let frame = pi_protocol::encode_client_message(&bad_hello, &Default::default()).unwrap();
    write.write_all(&frame).await.unwrap();
    write.flush().await.unwrap();

    // Read the response frame: must be a hello_error.
    let mut header = [0u8; 4];
    let n = read.read_exact(&mut header).await.unwrap_or(0);
    assert_eq!(n, 4, "header bytes: {:02x?}", header);
    let len = u32::from_be_bytes(header) as usize;
    assert!(len <= 16 * 1024 * 1024, "unreasonable frame length {len}");
    let mut body = vec![0u8; len];
    read.read_exact(&mut body).await.unwrap();
    let mut frame = header.to_vec();
    frame.extend_from_slice(&body);
    let messages = pi_protocol::ServerMessageDecoder::new(&Default::default())
        .unwrap()
        .push(&frame)
        .unwrap();
    assert!(matches!(
        messages[0],
        pi_protocol::ServerMessage::HelloError { .. }
    ));

    let _ = read;
    let _ = writer_shutdown(write).await;
    let _ = server.close().await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

async fn writer_shutdown(mut w: tokio::net::unix::OwnedWriteHalf) {
    use tokio::io::AsyncWriteExt;
    let _ = w.shutdown().await;
}

// ---------------------------------------------------------------------------
// Upstream oracle: upstream_pi/packages/server/src/testing/client.ts. This
// local client intentionally stays byte-oriented so every conformance case
// exercises the real Unix framing and server lifecycle without another network
// dependency.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ProtocolTestClient {
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    messages: Arc<Mutex<Vec<ServerMessage>>>,
    message_notify: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicBool>,
    close_notify: Arc<tokio::sync::Notify>,
    next_request: Arc<std::sync::atomic::AtomicUsize>,
}

impl ProtocolTestClient {
    async fn connect(path: &str) -> Self {
        let stream = tokio::net::UnixStream::connect(path).await.unwrap();
        let (mut reader, writer) = stream.into_split();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let message_notify = Arc::new(tokio::sync::Notify::new());
        let closed = Arc::new(AtomicBool::new(false));
        let close_notify = Arc::new(tokio::sync::Notify::new());
        let read_messages = messages.clone();
        let read_notify = message_notify.clone();
        let read_closed = closed.clone();
        let read_close_notify = close_notify.clone();
        tokio::spawn(async move {
            let mut decoder = ServerMessageDecoder::new(&FrameDecoderOptions::default()).unwrap();
            let mut buffer = vec![0u8; 4096];
            loop {
                let read = match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let decoded = match decoder.push(&buffer[..read]) {
                    Ok(messages) => messages,
                    Err(_) => break,
                };
                if !decoded.is_empty() {
                    read_messages.lock().unwrap().extend(decoded);
                    read_notify.notify_waiters();
                }
            }
            read_closed.store(true, Ordering::SeqCst);
            read_close_notify.notify_waiters();
            read_notify.notify_waiters();
        });
        Self {
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            messages,
            message_notify,
            closed,
            close_notify,
            next_request: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        }
    }

    async fn send_bytes(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().await;
        writer.write_all(bytes).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn send_fragmented(&self, bytes: &[u8], split_at: usize) {
        let split_at = split_at.min(bytes.len());
        let mut writer = self.writer.lock().await;
        writer.write_all(&bytes[..split_at]).await.unwrap();
        writer.flush().await.unwrap();
        tokio::task::yield_now().await;
        writer.write_all(&bytes[split_at..]).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn send_message(&self, message: &ClientMessage) {
        let frame = encode_client_message(message, &FrameDecoderOptions::default()).unwrap();
        self.send_bytes(&frame).await;
    }

    async fn send_messages(&self, messages: &[ClientMessage]) {
        let mut combined = Vec::new();
        for message in messages {
            combined
                .extend(encode_client_message(message, &FrameDecoderOptions::default()).unwrap());
        }
        self.send_bytes(&combined).await;
    }

    async fn hello(&self) -> ServerMessage {
        self.send_message(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await;
        self.next(|message| {
            matches!(
                message,
                ServerMessage::Hello { .. } | ServerMessage::HelloError { .. }
            )
        })
        .await
    }

    async fn request(&self, command: Command) -> ServerMessage {
        let id = format!(
            "request-{}",
            self.next_request.fetch_add(1, Ordering::SeqCst)
        );
        self.request_with_id(&id, command).await
    }

    async fn request_with_id(&self, id: &str, command: Command) -> ServerMessage {
        self.send_message(&ClientMessage::Request {
            id: id.to_string(),
            request: command,
        })
        .await;
        let id = id.to_string();
        self.next(move |message| {
            matches!(message, ServerMessage::Response { id: response_id, .. } if response_id == &id)
        })
        .await
    }

    fn begin_request(&self, id: &str, command: Command) -> tokio::task::JoinHandle<ServerMessage> {
        let client = self.clone();
        let id = id.to_string();
        tokio::spawn(async move { client.request_with_id(&id, command).await })
    }

    async fn next<F>(&self, predicate: F) -> ServerMessage
    where
        F: Fn(&ServerMessage) -> bool,
    {
        self.next_from(0, predicate).await
    }

    async fn next_from<F>(&self, index: usize, predicate: F) -> ServerMessage
    where
        F: Fn(&ServerMessage) -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(message) = self
                .messages
                .lock()
                .unwrap()
                .iter()
                .skip(index)
                .find(|message| predicate(message))
                .cloned()
            {
                return message;
            }
            if self.closed.load(Ordering::SeqCst) {
                panic!("wire client closed before expected message");
            }
            let notified = self.message_notify.notified();
            if let Some(message) = self
                .messages
                .lock()
                .unwrap()
                .iter()
                .skip(index)
                .find(|message| predicate(message))
                .cloned()
            {
                return message;
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .expect("timed out waiting for protocol message");
        }
    }

    async fn wait_for_close(&self) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.close_notify.notified();
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::timeout(Duration::from_secs(3), notified)
            .await
            .expect("timed out waiting for wire close");
    }

    fn messages(&self) -> Vec<ServerMessage> {
        self.messages.lock().unwrap().clone()
    }

    async fn close(&self) {
        let mut writer = self.writer.lock().await;
        let _ = writer.shutdown().await;
    }
}

struct RunningTestServer {
    server: PiServer,
    directory: PathBuf,
    socket: String,
}

impl RunningTestServer {
    async fn connect(&self) -> ProtocolTestClient {
        ProtocolTestClient::connect(&self.socket).await
    }

    async fn stop(mut self, clients: &[ProtocolTestClient]) {
        for client in clients {
            client.close().await;
        }
        self.server.close().await.unwrap();
        tokio::fs::remove_dir_all(self.directory).await.unwrap();
    }
}

async fn start_test_server<S: PiServerService + 'static>(
    service: S,
    max_frame_length: Option<u64>,
    handshake_timeout_ms: Option<u64>,
    on_error: Option<Arc<dyn Fn(std::io::Error) + Send + Sync>>,
) -> RunningTestServer {
    let directory =
        std::env::temp_dir().join(format!("pi-server-conformance-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let socket = directory.join("server.sock").to_string_lossy().into_owned();
    let mut server = PiServer::new(
        Box::new(service),
        pi_server::PiServerOptions {
            listeners: vec![Box::new(
                pi_server::UnixListener::new(socket.clone()).unwrap(),
            )],
            max_frame_length,
            handshake_timeout_ms,
            server_id: Some("conformance-server".to_string()),
            on_error,
        },
    )
    .unwrap();
    server.start().await.unwrap();
    RunningTestServer {
        server,
        directory,
        socket,
    }
}

fn response_error(message: ServerMessage) -> pi_protocol::ProtocolError {
    match message {
        ServerMessage::Response {
            ok: false,
            error: Some(error),
            ..
        }
        | ServerMessage::HelloError { error } => error,
        other => panic!("expected protocol error, got {other:?}"),
    }
}

fn response_session(message: ServerMessage) -> pi_protocol::SessionSnapshot {
    match message {
        ServerMessage::Response {
            ok: true,
            result:
                Some(
                    CommandResult::Create { session }
                    | CommandResult::Attach { session }
                    | CommandResult::Prompt { session }
                    | CommandResult::Steer { session }
                    | CommandResult::Abort { session }
                    | CommandResult::SetModel { session }
                    | CommandResult::SetThinking { session },
                ),
            ..
        } => session,
        other => panic!("expected session response, got {other:?}"),
    }
}

fn transcript_has_assistant_text(item: &pi_protocol::TranscriptItem, expected: &str) -> bool {
    match item {
        pi_protocol::TranscriptItem::Assistant(item) => item.content.iter().any(|content| {
            matches!(
                content,
                pi_protocol::AssistantContent::Text(pi_protocol::TextContent::Text { text })
                    if text == expected
            )
        }),
        // `TranscriptItem` is an untagged serde union in the local protocol
        // crate, so a wire assistant text item can decode through its first
        // (user) variant. Preserve the role assertion while accepting that
        // local decoder representation; the encoded role remains assistant.
        pi_protocol::TranscriptItem::User(item) if item.role == "assistant" => {
            item.content.iter().any(|content| {
                matches!(
                    content,
                    pi_protocol::UserContent::Text(pi_protocol::TextContent::Text { text })
                        if text == expected
                )
            })
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Upstream oracle group: upstream_pi/packages/server/test/conformance.test.ts
// transport, malformed-frame, handshake, response-correlation, and cleanup
// cases, with framing from upstream_pi/packages/server/src/connection.ts.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn testing_deferred_resolves_once_and_supports_repeated_waiters() {
    let deferred = Deferred::new();
    let first = deferred.clone();
    let second = deferred.clone();
    let waiter_one = tokio::spawn(async move { first.wait().await });
    let waiter_two = tokio::spawn(async move { second.wait().await });
    tokio::task::yield_now().await;
    deferred.resolve("done".to_string());
    deferred.resolve("ignored".to_string());
    assert_eq!(waiter_one.await.unwrap(), "done");
    assert_eq!(waiter_two.await.unwrap(), "done");
    assert!(deferred.is_resolved());
    assert_eq!(deferred.promise().await, "done");
}

#[tokio::test]
async fn testing_runtime_exposes_deferred_prompt_and_abort_controls() {
    let service = TestServerService::new();
    let mut service_object: Box<dyn PiServerService> = Box::new(service.clone());
    let runtime = service_object
        .create_session(pi_server::types::CreateSessionOptions {
            id: "runtime".to_string(),
            cwd: None,
            name: None,
            model: None,
            thinking_level: None,
        })
        .unwrap();
    let mut typed = service.latest_runtime_expect("runtime");
    runtime
        .lock()
        .unwrap()
        .prompt(pi_server::PromptInput {
            text: "hello".to_string(),
        })
        .unwrap();
    let pending = runtime.lock().unwrap().take_pending_operation().unwrap();
    assert_eq!(
        typed.snapshot().unwrap().phase,
        pi_protocol::SessionPhase::Turn
    );
    typed.abort().unwrap();
    pending.wait().await.unwrap();
    let snapshot = typed.snapshot().unwrap();
    assert_eq!(snapshot.phase, pi_protocol::SessionPhase::Idle);
    assert!(matches!(
        snapshot.transcript.last(),
        Some(pi_protocol::TranscriptItem::Assistant(item))
            if item.status == pi_protocol::AssistantStatus::Aborted
    ));
}

#[tokio::test]
async fn fragmented_hello_and_correlated_list_follow_test_client_contract() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    let hello = encode_client_message(
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        &FrameDecoderOptions::default(),
    )
    .unwrap();
    client.send_fragmented(&hello, 2).await;
    assert!(matches!(
        client
            .next(|m| matches!(m, ServerMessage::Hello { .. }))
            .await,
        ServerMessage::Hello { .. }
    ));
    let response = client.request(Command::List).await;
    assert!(matches!(
        response,
        ServerMessage::Response {
            ok: true,
            result: Some(CommandResult::List { .. }),
            ..
        }
    ));
    running.stop(&[client]).await;
}

#[tokio::test]
async fn hello_snapshot_contains_seeded_sessions_and_models() {
    let service = TestServerService::new();
    service.seed("first");
    let running = start_test_server(service, None, None, None).await;
    let client = running.connect().await;
    let hello = client.hello().await;
    match hello {
        ServerMessage::Hello { snapshot, .. } => {
            assert_eq!(snapshot.revision, 0);
            assert_eq!(snapshot.sessions[0].id, "first");
            assert_eq!(snapshot.models[0].id, "small");
        }
        other => panic!("expected hello, got {other:?}"),
    }
    running.stop(&[client]).await;
}

#[tokio::test]
async fn request_before_hello_is_rejected_and_connection_closes() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    client
        .send_message(&ClientMessage::Request {
            id: "early".to_string(),
            request: Command::List,
        })
        .await;
    let error = response_error(
        client
            .next(|m| matches!(m, ServerMessage::HelloError { .. }))
            .await,
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    client.wait_for_close().await;
    running.stop(&[client]).await;
}

#[tokio::test]
async fn duplicate_hello_is_rejected_after_handshake() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    assert!(matches!(client.hello().await, ServerMessage::Hello { .. }));
    client
        .send_message(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await;
    let error = response_error(
        client
            .next(|m| matches!(m, ServerMessage::HelloError { .. }))
            .await,
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    client.wait_for_close().await;
    running.stop(&[client]).await;
}

#[tokio::test]
async fn handshake_timeout_emits_invalid_request_and_closes() {
    let running = start_test_server(TestServerService::new(), None, Some(20), None).await;
    let client = running.connect().await;
    let error = response_error(
        client
            .next(|m| matches!(m, ServerMessage::HelloError { .. }))
            .await,
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    assert_eq!(error.message, "Handshake timeout");
    client.wait_for_close().await;
    running.stop(&[client]).await;
}

#[tokio::test]
async fn malformed_cbor_frame_maps_to_hello_error_and_close() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    client.send_bytes(&encode_frame(&[0xff]).unwrap()).await;
    let error = response_error(
        client
            .next(|m| matches!(m, ServerMessage::HelloError { .. }))
            .await,
    );
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    client.wait_for_close().await;
    running.stop(&[client]).await;
}

#[tokio::test]
async fn oversized_frame_is_rejected_without_handshake() {
    let running = start_test_server(TestServerService::new(), Some(128), None, None).await;
    let client = running.connect().await;
    let mut frame = vec![0u8; 4 + 129];
    frame[3] = 129;
    client.send_bytes(&frame).await;
    client.wait_for_close().await;
    assert!(!client
        .messages()
        .iter()
        .any(|message| matches!(message, ServerMessage::Hello { .. })));
    running.stop(&[client]).await;
}

#[tokio::test]
async fn outbound_frame_limit_closes_without_emitting_partial_hello() {
    let running = start_test_server(TestServerService::new(), Some(128), None, None).await;
    let client = running.connect().await;
    client
        .send_message(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await;
    client.wait_for_close().await;
    assert!(client.messages().is_empty());
    running.stop(&[client]).await;
}

#[tokio::test]
async fn multiple_framed_requests_in_one_chunk_keep_each_id() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .send_messages(&[
            ClientMessage::Request {
                id: "first".to_string(),
                request: Command::List,
            },
            ClientMessage::Request {
                id: "second".to_string(),
                request: Command::List,
            },
        ])
        .await;
    assert!(matches!(
        client
            .next(|m| matches!(m, ServerMessage::Response { id, ok: true, .. } if id == "first"))
            .await,
        ServerMessage::Response { .. }
    ));
    assert!(matches!(
        client
            .next(|m| matches!(m, ServerMessage::Response { id, ok: true, .. } if id == "second"))
            .await,
        ServerMessage::Response { .. }
    ));
    running.stop(&[client]).await;
}

#[tokio::test]
async fn hello_and_request_in_one_chunk_are_dispatched_after_handshake() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    client
        .send_messages(&[
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            },
            ClientMessage::Request {
                id: "queued".to_string(),
                request: Command::List,
            },
        ])
        .await;
    assert!(matches!(
        client
            .next(|m| matches!(m, ServerMessage::Hello { .. }))
            .await,
        ServerMessage::Hello { .. }
    ));
    assert!(matches!(
        client
            .next(|m| matches!(m, ServerMessage::Response { id, ok: true, .. } if id == "queued"))
            .await,
        ServerMessage::Response { .. }
    ));
    running.stop(&[client]).await;
}

#[tokio::test]
async fn server_snapshot_revisions_are_serialized_after_mutations() {
    let running = start_test_server(TestServerService::new(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Create {
            cwd: None,
            name: Some("first".to_string()),
            model: None,
            thinking_level: None,
        })
        .await;
    client.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::ServerSnapshot { snapshot } } if snapshot.revision == 1)).await;
    client
        .request(Command::Create {
            cwd: None,
            name: Some("second".to_string()),
            model: None,
            thinking_level: None,
        })
        .await;
    client.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::ServerSnapshot { snapshot } } if snapshot.revision == 2)).await;
    let revisions = client
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            ServerMessage::Event {
                event: ServerEvent::ServerSnapshot { snapshot },
            } => Some(snapshot.revision),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(revisions, vec![1, 2]);
    running.stop(&[client]).await;
}

// ---------------------------------------------------------------------------
// Upstream oracle group: upstream_pi/packages/server/test/sessions.test.ts and
// upstream_pi/packages/server/src/testing/service.ts attachment, snapshots,
// exclusivity, deferred command queue, and disposal semantics; lifecycle
// normalization follows upstream_pi/packages/server/src/sessions.ts and
// upstream_pi/packages/server/src/snapshots.ts.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_assigns_a_durable_id_and_preserves_operation_metadata() {
    let service = TestServerService::new();
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    let created = client
        .request(Command::Create {
            cwd: Some("/work".to_string()),
            name: Some("Created".to_string()),
            model: None,
            thinking_level: None,
        })
        .await;
    let snapshot = response_session(created.clone());
    assert_eq!(Some(snapshot.id.clone()), service.last_created_id());
    assert_eq!(snapshot.cwd, "/work");
    assert_eq!(snapshot.name.as_deref(), Some("Created"));
    assert!(snapshot.attached);
    assert!(snapshot.locked);
    let listed = client.request(Command::List).await;
    match listed {
        ServerMessage::Response {
            result: Some(CommandResult::List { sessions }),
            ..
        } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, snapshot.id);
        }
        other => panic!("expected list, got {other:?}"),
    }
    running.stop(&[client]).await;
}

#[tokio::test]
async fn attach_reuses_one_runtime_and_returns_locked_attached_snapshot() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let first = running.connect().await;
    let second = running.connect().await;
    first.hello().await;
    second.hello().await;
    let first_snapshot = response_session(
        first
            .request(Command::Attach {
                session_id: "session-1".to_string(),
            })
            .await,
    );
    let second_snapshot = response_session(
        second
            .request(Command::Attach {
                session_id: "session-1".to_string(),
            })
            .await,
    );
    assert!(first_snapshot.attached && first_snapshot.locked);
    assert!(second_snapshot.attached && second_snapshot.locked);
    assert_eq!(service.runtime_count("session-1"), 1);
    running.stop(&[first, second]).await;
}

#[tokio::test]
async fn detach_is_idempotent_and_disposes_only_after_last_attachment() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let first = running.connect().await;
    let second = running.connect().await;
    first.hello().await;
    second.hello().await;
    first
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    second
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    let runtime = service.latest_runtime_expect("session-1");
    first
        .request(Command::Detach {
            session_id: "session-1".to_string(),
        })
        .await;
    assert_eq!(runtime.dispose_count(), 0);
    second
        .request(Command::Detach {
            session_id: "session-1".to_string(),
        })
        .await;
    runtime.disposed().wait().await;
    assert_eq!(runtime.dispose_count(), 1);
    second
        .request(Command::Detach {
            session_id: "session-1".to_string(),
        })
        .await;
    assert_eq!(runtime.dispose_count(), 1);
    running.stop(&[first, second]).await;
}

#[tokio::test]
async fn control_commands_require_attachment_and_map_service_lock_errors() {
    let service = TestServerService::new();
    service.seed("locked");
    service.lock_session("locked");
    let running = start_test_server(service, None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    let locked = response_error(
        client
            .request(Command::Attach {
                session_id: "locked".to_string(),
            })
            .await,
    );
    assert_eq!(locked.code, ProtocolErrorCode::SessionLocked);
    let unattached = response_error(
        client
            .request(Command::Abort {
                session_id: "locked".to_string(),
            })
            .await,
    );
    assert_eq!(unattached.code, ProtocolErrorCode::InvalidRequest);
    running.stop(&[client]).await;
}

#[tokio::test]
async fn prompt_queue_rejects_second_prompt_but_allows_steer_and_abort() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    let prompt = client.begin_request(
        "prompt",
        Command::Prompt {
            session_id: "session-1".to_string(),
            text: "first".to_string(),
        },
    );
    client.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::SessionSnapshot { snapshot } } if snapshot.phase == pi_protocol::SessionPhase::Turn)).await;
    let busy = response_error(
        client
            .request(Command::Prompt {
                session_id: "session-1".to_string(),
                text: "second".to_string(),
            })
            .await,
    );
    assert_eq!(busy.code, ProtocolErrorCode::Busy);
    let steer = client
        .request(Command::Steer {
            session_id: "session-1".to_string(),
            text: "adjust".to_string(),
        })
        .await;
    assert!(matches!(steer, ServerMessage::Response { ok: true, .. }));
    assert_eq!(
        service.latest_runtime_expect("session-1").steers()[0].text,
        "adjust"
    );
    let abort = client
        .request(Command::Abort {
            session_id: "session-1".to_string(),
        })
        .await;
    assert!(matches!(abort, ServerMessage::Response { ok: true, .. }));
    let completed = prompt.await.unwrap();
    assert!(
        matches!(completed, ServerMessage::Response { ok: true, result: Some(CommandResult::Prompt { session }), .. } if session.phase == pi_protocol::SessionPhase::Idle)
    );
    running.stop(&[client]).await;
}

#[tokio::test]
async fn abort_response_can_overtake_a_deferred_prompt_response() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    let prompt = client.begin_request(
        "slow",
        Command::Prompt {
            session_id: "session-1".to_string(),
            text: "first".to_string(),
        },
    );
    client.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::SessionSnapshot { snapshot } } if snapshot.phase == pi_protocol::SessionPhase::Turn)).await;
    client
        .send_message(&ClientMessage::Request {
            id: "abort".to_string(),
            request: Command::Abort {
                session_id: "session-1".to_string(),
            },
        })
        .await;
    let abort = client
        .next(|m| matches!(m, ServerMessage::Response { id, ok: true, .. } if id == "abort"))
        .await;
    assert!(matches!(abort, ServerMessage::Response { .. }));
    let prompt_response = prompt.await.unwrap();
    assert!(
        matches!(prompt_response, ServerMessage::Response { id, ok: true, .. } if id == "slow")
    );
    let ordered = client
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            ServerMessage::Response { id, .. } if id == "abort" || id == "slow" => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ordered, vec!["abort", "slow"]);
    running.stop(&[client]).await;
}

#[tokio::test]
async fn deferred_prompt_survives_disconnect_and_disposes_when_idle() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    client.begin_request(
        "prompt",
        Command::Prompt {
            session_id: "session-1".to_string(),
            text: "survive".to_string(),
        },
    );
    client.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::SessionSnapshot { snapshot } } if snapshot.phase == pi_protocol::SessionPhase::Turn)).await;
    let runtime = service.latest_runtime_expect("session-1");
    client.close().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(runtime.dispose_count(), 0);
    runtime.finish_prompt().unwrap();
    runtime.disposed().wait().await;
    assert_eq!(runtime.dispose_count(), 1);
    let reconnect = running.connect().await;
    reconnect.hello().await;
    let snapshot = response_session(
        reconnect
            .request(Command::Attach {
                session_id: "session-1".to_string(),
            })
            .await,
    );
    assert_eq!(snapshot.transcript.len(), 2);
    assert!(transcript_has_assistant_text(
        &snapshot.transcript[1],
        "reply:survive"
    ));
    running.stop(&[client, reconnect]).await;
}

#[tokio::test]
async fn prompt_response_reports_attachment_relative_to_requesting_connection() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let first = running.connect().await;
    let second = running.connect().await;
    first.hello().await;
    second.hello().await;
    first
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    second
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    let prompt = first.begin_request(
        "prompt",
        Command::Prompt {
            session_id: "session-1".to_string(),
            text: "hello".to_string(),
        },
    );
    first.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::SessionSnapshot { snapshot } } if snapshot.phase == pi_protocol::SessionPhase::Turn)).await;
    first
        .request(Command::Detach {
            session_id: "session-1".to_string(),
        })
        .await;
    service
        .latest_runtime_expect("session-1")
        .finish_prompt()
        .unwrap();
    assert!(
        matches!(prompt.await.unwrap(), ServerMessage::Response { ok: true, result: Some(CommandResult::Prompt { session }), .. } if !session.attached)
    );
    running.stop(&[first, second]).await;
}

#[tokio::test]
async fn progress_and_snapshot_events_are_scoped_to_attached_clients() {
    let service = TestServerService::new();
    service.seed("first");
    service.seed("second");
    let running = start_test_server(service.clone(), None, None, None).await;
    let first = running.connect().await;
    let second = running.connect().await;
    first.hello().await;
    second.hello().await;
    first
        .request(Command::Attach {
            session_id: "first".to_string(),
        })
        .await;
    second
        .request(Command::Attach {
            session_id: "second".to_string(),
        })
        .await;
    let baseline = first.messages().len();
    service.latest_runtime_expect("first").emit_progress(
        pi_protocol::TranscriptProgress::AssistantDelta {
            message_id: "assistant-1".to_string(),
            content_index: 0,
            kind: pi_protocol::TranscriptDeltaKind::Text,
            delta: "hello".to_string(),
        },
    );
    assert!(matches!(first.next(|m| matches!(m, ServerMessage::Event { event: ServerEvent::SessionProgress { session_id, .. } } if session_id == "first")).await, ServerMessage::Event { .. }));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!second.messages().iter().any(|message| matches!(
        message,
        ServerMessage::Event {
            event: ServerEvent::SessionProgress { .. }
        }
    )));
    service.latest_runtime_expect("first").emit_snapshot();
    first.next_from(baseline, |m| matches!(m, ServerMessage::Event { event: ServerEvent::SessionSnapshot { snapshot } } if snapshot.id == "first")).await;
    running.stop(&[first, second]).await;
}

#[tokio::test]
async fn detach_removes_runtime_subscriptions() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    let before = client
        .messages()
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                ServerMessage::Event {
                    event: ServerEvent::SessionSnapshot { .. }
                }
            )
        })
        .count();
    client
        .request(Command::Detach {
            session_id: "session-1".to_string(),
        })
        .await;
    service.latest_runtime_expect("session-1").emit_snapshot();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let after = client
        .messages()
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                ServerMessage::Event {
                    event: ServerEvent::SessionSnapshot { .. }
                }
            )
        })
        .count();
    assert_eq!(before, after);
    running.stop(&[client]).await;
}

#[tokio::test]
async fn terminal_runtime_error_closes_attached_clients_and_releases_lock() {
    let service = TestServerService::new();
    service.seed("terminal");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "terminal".to_string(),
        })
        .await;
    let runtime = service.latest_runtime_expect("terminal");
    runtime.set_phase(pi_protocol::SessionPhase::Turn);
    runtime.emit_error(PiServerError::new(
        ProtocolErrorCode::SessionLocked,
        "lock ownership lost",
    ));
    client.wait_for_close().await;
    runtime.disposed().wait().await;
    assert_eq!(runtime.dispose_count(), 1);
    assert!(!service.is_locked("terminal"));
    let next = running.connect().await;
    next.hello().await;
    assert!(matches!(
        next.request(Command::Attach {
            session_id: "terminal".to_string()
        })
        .await,
        ServerMessage::Response { ok: true, .. }
    ));
    running.stop(&[client, next]).await;
}

#[tokio::test]
async fn client_disconnect_disposes_an_idle_runtime() {
    let service = TestServerService::new();
    service.seed("session-1");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "session-1".to_string(),
        })
        .await;
    let runtime = service.latest_runtime_expect("session-1");
    client.close().await;
    runtime.disposed().wait().await;
    assert_eq!(runtime.dispose_count(), 1);
    running.stop(&[client]).await;
}

// ---------------------------------------------------------------------------
// Upstream oracle group: upstream_pi/packages/server/test/server.test.ts error
// normalization and listener/shutdown resource gates, with listener behavior
// from upstream_pi/packages/server/src/listener.ts. The wrappers below model
// service failures without exposing private implementation detail to the
// protocol client.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FailingListService {
    inner: TestServerService,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    code: ProtocolErrorCode,
    message: String,
}

impl PiServerService for FailingListService {
    fn list_sessions(&self) -> Result<Vec<pi_protocol::SessionMetadata>, PiServerError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            Err(PiServerError::new(self.code.clone(), self.message.clone()))
        } else {
            self.inner.list_sessions()
        }
    }

    fn list_models(&self) -> Result<Vec<pi_protocol::ModelMetadata>, PiServerError> {
        self.inner.list_models()
    }

    fn create_session(
        &mut self,
        options: pi_server::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        self.inner.create_session(options)
    }

    fn open_session(
        &mut self,
        session_id: String,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        self.inner.open_session(session_id)
    }
}

#[tokio::test]
async fn internal_service_errors_are_sanitized_and_reported() {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let errors_for_observer = errors.clone();
    let service = FailingListService {
        inner: TestServerService::new(),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        code: ProtocolErrorCode::InternalError,
        message: "private storage detail".to_string(),
    };
    let running = start_test_server(
        service,
        None,
        None,
        Some(Arc::new(move |error| {
            errors_for_observer.lock().unwrap().push(error.to_string());
        })),
    )
    .await;
    let client = running.connect().await;
    client.hello().await;
    let error = response_error(client.request(Command::List).await);
    assert_eq!(error.code, ProtocolErrorCode::InternalError);
    assert_eq!(error.message, "Internal server error");
    assert!(!error.message.contains("private"));
    assert!(errors
        .lock()
        .unwrap()
        .iter()
        .any(|message| message.contains("private")));
    running.stop(&[client]).await;
}

#[tokio::test]
async fn not_implemented_service_errors_keep_the_stable_message() {
    let service = FailingListService {
        inner: TestServerService::new(),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        code: ProtocolErrorCode::NotImplemented,
        message: "private alternate wording".to_string(),
    };
    let running = start_test_server(service, None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    let error = response_error(client.request(Command::List).await);
    assert_eq!(error.code, ProtocolErrorCode::NotImplemented);
    assert_eq!(error.message, "Operation is not implemented");
    running.stop(&[client]).await;
}

#[tokio::test]
async fn server_close_disposes_runtimes_and_removes_listener_resources() {
    let service = TestServerService::new();
    service.seed("first");
    let mut running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "first".to_string(),
        })
        .await;
    let runtime = service.latest_runtime_expect("first");
    let socket = running.socket.clone();
    running.server.close().await.unwrap();
    client.wait_for_close().await;
    runtime.disposed().wait().await;
    assert_eq!(runtime.dispose_count(), 1);
    assert!(running.server.addresses().is_empty());
    assert!(tokio::fs::symlink_metadata(&socket).await.is_err());
    running.server.close().await.unwrap();
    tokio::fs::remove_dir_all(running.directory).await.unwrap();
}

#[tokio::test]
async fn invalid_server_options_are_rejected_before_start() {
    let directory =
        std::env::temp_dir().join(format!("pi-server-options-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let socket = directory.join("server.sock").to_string_lossy().into_owned();
    let result = PiServer::new(
        Box::new(TestServerService::new()),
        pi_server::PiServerOptions {
            listeners: vec![Box::new(pi_server::UnixListener::new(socket).unwrap())],
            max_frame_length: Some(0),
            handshake_timeout_ms: None,
            server_id: Some("server".to_string()),
            on_error: None,
        },
    );
    let error = match result {
        Ok(_) => panic!("invalid frame limit was accepted"),
        Err(error) => error,
    };
    assert!(error.contains("max_frame_length"));
    tokio::fs::remove_dir_all(directory).await.unwrap();
}

#[tokio::test]
async fn terminal_cleanup_is_stable_when_client_and_server_close_race() {
    let service = TestServerService::new();
    service.seed("race");
    let running = start_test_server(service.clone(), None, None, None).await;
    let client = running.connect().await;
    client.hello().await;
    client
        .request(Command::Attach {
            session_id: "race".to_string(),
        })
        .await;
    let runtime = service.latest_runtime_expect("race");
    client.close().await;
    runtime.disposed().wait().await;
    running.stop(&[client]).await;
    assert_eq!(runtime.dispose_count(), 1);
}

// Keep these imports/types exercised by the fixture metadata above; the
// concrete test-client and service contracts are intentionally offline.
#[allow(dead_code)]
fn _fixture_contract_markers() {
    let _ = TestSessionRuntime::disposed;
    let _ = pi_protocol::ServerMessageDecoder::new(&FrameDecoderOptions::default());
}
