//! PiServer — port of `packages/server/src/server.ts`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_protocol::{
    encode_server_message, is_supported_protocol_version, ClientMessage, Command, CommandResult,
    FrameDecoderOptions, ProtocolError, ProtocolErrorCode, ServerEvent, ServerMessage,
    DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION,
};

use crate::connection::{
    is_terminal_connection, ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler,
    ConnectionState,
};
use crate::errors::{PiServerError, INTERNAL_SERVER_ERROR_MESSAGE, NOT_IMPLEMENTED_MESSAGE};
use crate::live_session::{ConnectionHandle, EventSink, LiveSessionManager};
use crate::snapshots::ServerSnapshotPublisher;
use crate::types::{ArcErrorObserver, PiServerOptions, PiServerService};

const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// Connection protocol handler.
pub struct ConnectionHandler {
    state: ConnectionState,
    sessions: Arc<LiveSessionManager>,
    snapshots: Arc<ServerSnapshotPublisher>,
    closing: Arc<AtomicBool>,
    connection_handle: Arc<Mutex<ConnectionHandle>>,
    frame_options: FrameDecoderOptions,
    on_error: Option<ArcErrorObserver>,
}

impl ConnectionHandler {
    fn receive(&mut self, chunk: &[u8], self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>) {
        if is_terminal_connection(&self.state) {
            return;
        }
        let messages = match self.state.decoder.push(chunk) {
            Ok(messages) => messages,
            Err(_) => {
                let _ = self.fail_protocol_sync(ProtocolError {
                    code: ProtocolErrorCode::InvalidRequest,
                    message: "Invalid protocol message".to_string(),
                    details: None,
                });
                return;
            }
        };
        for message in messages {
            if is_terminal_connection(&self.state) {
                return;
            }
            self.dispatch_message(message, self_arc);
        }
    }

    fn dispatch_message(
        &mut self,
        message: ClientMessage,
        self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>,
    ) {
        match self.state.stage {
            "awaitingHello" => match message {
                ClientMessage::Hello { version } => {
                    self.state.stage = "handshaking";
                    let arc = self_arc.clone();
                    tokio::spawn(run_handshake(arc, version));
                }
                _ => {
                    let _ = self.fail_protocol_sync(ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "The first client message must be hello".to_string(),
                        details: None,
                    });
                }
            },
            "handshaking" => match message {
                ClientMessage::Request { id, request } => {
                    // The upstream server waits for hello before dispatching
                    // requests. Retain requests received in the same transport
                    // turn so framing and ordering are deterministic.
                    self.state.pending_requests.push((id, request));
                }
                ClientMessage::Hello { .. } => {
                    let _ = self.fail_protocol_sync(ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "hello may only be sent as the first message".to_string(),
                        details: None,
                    });
                }
            },
            "ready" => match message {
                ClientMessage::Request { id, request } => {
                    let arc = self_arc.clone();
                    tokio::spawn(run_request(arc, id, request));
                }
                ClientMessage::Hello { .. } => {
                    let _ = self.fail_protocol_sync(ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "hello may only be sent as the first message".to_string(),
                        details: None,
                    });
                }
            },
            _ => {}
        }
    }

    fn fail_protocol_sync(&mut self, error: ProtocolError) -> Result<(), String> {
        if is_terminal_connection(&self.state) {
            return Ok(());
        }
        self.state.stage = "closing";
        let frame =
            encode_server_message(&ServerMessage::HelloError { error }, &self.frame_options).ok();
        self.state.disconnected = true;
        self.state.stage = "closed";
        let connection = self.state.connection.clone();
        let sessions = self.sessions.clone();
        let snapshots = self.snapshots.clone();
        let connection_handle = self.connection_handle.clone();
        tokio::spawn(async move {
            let _ = connection.close(frame).await;
            let mut handle = connection_handle.lock().unwrap();
            handle.disconnected = true;
            handle.closed = true;
            handle.ready = false;
            sessions.disconnect(&mut handle);
            snapshots.revoke_connection_for(&connection);
        });
        Ok(())
    }

    fn report_error(&self, message: impl Into<String>) {
        report_error(&self.on_error, message.into());
    }
}

/// Handshake future: sends the hello snapshot, registers the connection only
/// after the send succeeds, catches up a concurrent revision, then releases
/// requests that arrived in the same transport chunk.
async fn run_handshake(arc: Arc<Mutex<dyn ByteConnectionHandler>>, version: u64) {
    if !is_supported_protocol_version(version) {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        let _ = handler.fail_protocol_sync(ProtocolError {
            code: ProtocolErrorCode::Version,
            message: format!("Unsupported protocol version {version}; expected {PROTOCOL_VERSION}"),
            details: None,
        });
        return;
    }

    let (connection, connection_id, snapshot, frame_options, snapshots) = {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        (
            handler.state.connection.clone(),
            handler.state.id.clone(),
            handler.snapshots.get(),
            handler.frame_options.clone(),
            handler.snapshots.clone(),
        )
    };
    let message = ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        connection_id,
        snapshot: snapshot.clone(),
    };
    let frame = match encode_server_message(&message, &frame_options) {
        Ok(frame) => frame,
        Err(error) => {
            let mut guard = arc.lock().unwrap();
            let handler = guard.as_connection_handler().unwrap();
            handler.report_error(error.to_string());
            let connection = handler.state.connection.clone();
            tokio::spawn(async move {
                let _ = connection.close(None).await;
            });
            return;
        }
    };
    if connection.send(&frame).await.is_err() {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        handler.report_error("Unix connection closed during handshake");
        let connection = handler.state.connection.clone();
        tokio::spawn(async move {
            let _ = connection.close(None).await;
        });
        return;
    }

    let (pending, catchup) = {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        if handler.state.disconnected || handler.state.stage != "handshaking" {
            return;
        }
        handler.state.handshake_complete = true;
        handler.state.stage = "ready";
        handler.connection_handle.lock().unwrap().ready = true;
        snapshots.register_connection(connection.clone());
        let catchup = if snapshots.current_revision() != snapshot.revision {
            encode_server_message(
                &ServerMessage::Event {
                    event: ServerEvent::ServerSnapshot {
                        snapshot: snapshots.get(),
                    },
                },
                &handler.frame_options,
            )
            .ok()
        } else {
            None
        };
        (std::mem::take(&mut handler.state.pending_requests), catchup)
    };
    if let Some(frame) = catchup {
        let _ = connection.send(&frame).await;
    }
    for (id, request) in pending {
        let request_arc = arc.clone();
        tokio::spawn(run_request(request_arc, id, request));
    }
}

/// Request future: delegates lifecycle and session semantics to
/// `LiveSessionManager`, then writes only the correlated response. Requests
/// remain independently scheduled so a deferred prompt does not block steer
/// or abort on the same connection.
async fn run_request(arc: Arc<Mutex<dyn ByteConnectionHandler>>, id: String, request: Command) {
    let (connection, sessions, connection_handle, snapshots, closing, frame_options, on_error) = {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        (
            handler.state.connection.clone(),
            handler.sessions.clone(),
            handler.connection_handle.clone(),
            handler.snapshots.clone(),
            handler.closing.clone(),
            handler.frame_options.clone(),
            handler.on_error.clone(),
        )
    };
    let result = sessions
        .execute_command_async(connection_handle, request)
        .await;
    let message = match &result {
        Ok(result) => ServerMessage::Response {
            id,
            ok: true,
            result: Some(result.clone()),
            error: None,
        },
        Err(error) => ServerMessage::Response {
            id,
            ok: false,
            result: None,
            error: Some(protocol_error(error, &on_error)),
        },
    };
    let frame = match encode_server_message(&message, &frame_options) {
        Ok(frame) => frame,
        Err(error) => {
            report_error(&on_error, error.to_string());
            let _ = connection.close(None).await;
            return;
        }
    };
    if connection.send(&frame).await.is_err() {
        report_error(
            &on_error,
            "Unix connection closed during response".to_string(),
        );
        let _ = connection.close(None).await;
        return;
    }
    if result.is_ok() && !closing.load(Ordering::SeqCst) {
        snapshots.broadcast().await;
    }
}

type ConnectionList = Arc<Mutex<Vec<Arc<Mutex<dyn ByteConnectionHandler>>>>>;

/// PiServer: owns listeners, connections, the lifecycle manager, and the
/// snapshot publisher.
pub struct PiServer {
    pub id: String,
    sessions: Arc<LiveSessionManager>,
    connections: ConnectionList,
    snapshots: Arc<ServerSnapshotPublisher>,
    listeners: Vec<Box<dyn crate::listener::PiServerListener>>,
    closing: Arc<AtomicBool>,
    frame_options: FrameDecoderOptions,
    handshake_timeout_ms: u64,
    on_error: Option<ArcErrorObserver>,
}

impl PiServer {
    pub fn new(
        service: Box<dyn PiServerService>,
        options: PiServerOptions,
    ) -> Result<Self, String> {
        if options.server_id.as_deref() == Some("") {
            return Err("PiServer server_id must not be empty".to_string());
        }
        let max_frame_length = options
            .max_frame_length
            .unwrap_or(DEFAULT_MAX_FRAME_LENGTH as u64);
        if max_frame_length == 0 || max_frame_length > u32::MAX as u64 {
            return Err(format!(
                "PiServer max_frame_length must be an integer between 1 and {}",
                u32::MAX
            ));
        }
        let handshake_timeout_ms = options
            .handshake_timeout_ms
            .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
        if handshake_timeout_ms == 0 || handshake_timeout_ms > MAX_TIMER_DELAY_MS {
            return Err(format!(
                "PiServer handshake_timeout_ms must be an integer between 1 and {MAX_TIMER_DELAY_MS}"
            ));
        }
        let id = options
            .server_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let service_box = service;
        let models = service_box.list_models().map_err(|e| e.to_string())?;
        let sessions = service_box.list_sessions().map_err(|e| e.to_string())?;
        let snapshots = Arc::new(ServerSnapshotPublisher::new(
            id.clone(),
            PROTOCOL_VERSION,
            models,
        ));
        snapshots.initialize(sessions);
        let service: Arc<Mutex<dyn PiServerService>> = Arc::new(Mutex::new(service_box));
        let closing = Arc::new(AtomicBool::new(false));
        let manager = Arc::new(LiveSessionManager::new_with_closing(
            service,
            snapshots.clone(),
            closing.clone(),
        ));
        if let Some(observer) = options.on_error.clone() {
            manager.set_error_reporter(Arc::new(move |error| {
                report_error(&Some(observer.clone()), error.to_string());
            }));
        }
        Ok(Self {
            id,
            sessions: manager,
            connections: Arc::new(Mutex::new(Vec::new())),
            snapshots,
            listeners: options.listeners,
            closing,
            frame_options: FrameDecoderOptions {
                max_frame_length: Some(max_frame_length as usize),
            },
            handshake_timeout_ms,
            on_error: options.on_error,
        })
    }

    pub fn snapshot_publisher(&self) -> Arc<ServerSnapshotPublisher> {
        self.snapshots.clone()
    }

    pub async fn start(&mut self) -> Result<(), String> {
        if self.closing.load(Ordering::SeqCst) {
            return Err("PiServer is closing or closed".to_string());
        }
        let accept = self.make_acceptor();
        for listener in &mut self.listeners {
            listener.start(accept.clone()).await?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<(), String> {
        if self.closing.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.sessions.mark_closing();
        for listener in &mut self.listeners {
            let _ = listener.close().await;
        }
        let connections = self.connections.lock().unwrap().clone();
        for connection in connections {
            close_handler(connection).await;
        }
        self.sessions.close();
        self.connections.lock().unwrap().clear();
        Ok(())
    }

    pub fn addresses(&self) -> Vec<String> {
        self.listeners.iter().filter_map(|l| l.address()).collect()
    }

    fn make_acceptor(&self) -> ByteConnectionAcceptor {
        let connections = self.connections.clone();
        let snapshots = self.snapshots.clone();
        let sessions = self.sessions.clone();
        let closing = self.closing.clone();
        let frame_options = self.frame_options.clone();
        let handshake_timeout_ms = self.handshake_timeout_ms;
        let on_error = self.on_error.clone();
        Arc::new(move |connection: Arc<dyn ByteConnection>| {
            let id = uuid::Uuid::new_v4().to_string();
            let mut connection_handle = ConnectionHandle::new(id.clone());
            connection_handle.ready = false;
            let event_connection = connection.clone();
            let event_options = frame_options.clone();
            let event_sink: EventSink = Arc::new(move |event| {
                let connection = event_connection.clone();
                let options = event_options.clone();
                tokio::spawn(async move {
                    let message = ServerMessage::Event { event };
                    if let Ok(frame) = encode_server_message(&message, &options) {
                        let _ = connection.send(&frame).await;
                    }
                });
            });
            connection_handle.events = Some(event_sink);
            let close_connection = connection.clone();
            connection_handle.close = Some(Arc::new(move || {
                let connection = close_connection.clone();
                tokio::spawn(async move {
                    let _ = connection.close(None).await;
                });
            }));
            let connection_handle = Arc::new(Mutex::new(connection_handle));
            let decoder = pi_protocol::ClientMessageDecoder::new(&frame_options)
                .expect("validated client message decoder options");
            let state = ConnectionState::new(id, connection.clone(), decoder);
            let handler = Arc::new(Mutex::new(ConnectionHandler {
                state,
                sessions: sessions.clone(),
                snapshots: snapshots.clone(),
                closing: closing.clone(),
                connection_handle,
                frame_options: frame_options.clone(),
                on_error: on_error.clone(),
            }));
            connections.lock().unwrap().push(handler.clone());
            let timer_handler = handler.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(handshake_timeout_ms)).await;
                let mut guard = timer_handler.lock().unwrap();
                let handler = guard.as_connection_handler().unwrap();
                if handler.state.stage == "awaitingHello" || handler.state.stage == "handshaking" {
                    let _ = handler.fail_protocol_sync(ProtocolError {
                        code: ProtocolErrorCode::InvalidRequest,
                        message: "Handshake timeout".to_string(),
                        details: None,
                    });
                }
            });
            handler as Arc<Mutex<dyn ByteConnectionHandler>>
        })
    }
}

impl ByteConnectionHandler for ConnectionHandler {
    fn on_data(&mut self, chunk: &[u8], self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>) {
        self.receive(chunk, self_arc);
    }

    fn as_connection_handler(&mut self) -> Option<&mut ConnectionHandler> {
        Some(self)
    }

    fn on_close(&mut self) {
        let handshake_complete = self.state.handshake_complete;
        self.state.disconnected = true;
        self.state.stage = "closed";
        {
            let mut handle = self.connection_handle.lock().unwrap();
            handle.disconnected = true;
            handle.closed = true;
            handle.ready = false;
            self.sessions.disconnect(&mut handle);
        }
        self.snapshots.revoke_connection_for(&self.state.connection);
        if !self.closing.load(Ordering::SeqCst) && handshake_complete {
            let snapshots = self.snapshots.clone();
            tokio::spawn(async move {
                snapshots.broadcast().await;
            });
        }
    }

    fn on_error(&mut self, error: String) {
        self.report_error(error);
        let connection = self.state.connection.clone();
        tokio::spawn(async move {
            let _ = connection.close(None).await;
        });
    }
}

async fn close_handler(handler: Arc<Mutex<dyn ByteConnectionHandler>>) {
    let (connection, sessions, connection_handle, snapshots) = {
        let mut guard = handler.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        handler.state.stage = "closing";
        handler.state.disconnected = true;
        (
            handler.state.connection.clone(),
            handler.sessions.clone(),
            handler.connection_handle.clone(),
            handler.snapshots.clone(),
        )
    };
    let _ = connection.close(None).await;
    let mut handle = connection_handle.lock().unwrap();
    handle.disconnected = true;
    handle.closed = true;
    handle.ready = false;
    sessions.disconnect(&mut handle);
    snapshots.revoke_connection_for(&connection);
}

fn report_error(observer: &Option<ArcErrorObserver>, message: String) {
    if let Some(observer) = observer {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer(std::io::Error::other(message));
        }));
    }
}

fn protocol_error(error: &PiServerError, observer: &Option<ArcErrorObserver>) -> ProtocolError {
    if error.code == ProtocolErrorCode::NotImplemented {
        return ProtocolError {
            code: ProtocolErrorCode::NotImplemented,
            message: NOT_IMPLEMENTED_MESSAGE.to_string(),
            details: None,
        };
    }
    if error.code == ProtocolErrorCode::InternalError {
        report_error(observer, error.to_string());
        return ProtocolError {
            code: ProtocolErrorCode::InternalError,
            message: INTERNAL_SERVER_ERROR_MESSAGE.to_string(),
            details: None,
        };
    }
    error.into_protocol()
}

/// Session snapshot produced by a mutating command, if any.
pub fn session_snapshot_of(result: &CommandResult) -> Option<pi_protocol::SessionSnapshot> {
    match result {
        CommandResult::Create { session }
        | CommandResult::Attach { session }
        | CommandResult::Prompt { session }
        | CommandResult::Steer { session }
        | CommandResult::Abort { session }
        | CommandResult::SetModel { session }
        | CommandResult::SetThinking { session } => Some(session.clone()),
        CommandResult::Detach { .. } | CommandResult::List { .. } => None,
    }
}

/// Execute a protocol Command directly against a service. Kept as a small
/// embedders' seam; the wire server uses `LiveSessionManager` so attach and
/// command lifecycle state cannot be bypassed.
pub fn run_command_sync(
    service: &mut dyn PiServerService,
    command: Command,
    snapshots: &Arc<ServerSnapshotPublisher>,
) -> Result<CommandResult, PiServerError> {
    match command {
        Command::List => {
            let sessions = service.list_sessions()?;
            Ok(CommandResult::List { sessions })
        }
        Command::Create {
            cwd,
            name,
            model,
            thinking_level,
        } => {
            let id = uuid::Uuid::new_v4().to_string();
            let runtime = service.create_session(crate::types::CreateSessionOptions {
                id,
                cwd,
                name,
                model,
                thinking_level,
            })?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Create { session: snapshot })
        }
        Command::Attach { session_id } => {
            let runtime = service.open_session(session_id)?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            Ok(CommandResult::Attach { session: snapshot })
        }
        Command::Detach { session_id } => Ok(CommandResult::Detach { session_id }),
        Command::Prompt { session_id, text } => {
            let runtime = service.open_session(session_id)?;
            runtime
                .lock()
                .unwrap()
                .prompt(crate::types::PromptInput { text })?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Prompt { session: snapshot })
        }
        Command::Steer { session_id, text } => {
            let runtime = service.open_session(session_id)?;
            runtime
                .lock()
                .unwrap()
                .steer(crate::types::SteerInput { text })?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Steer { session: snapshot })
        }
        Command::Abort { session_id } => {
            let runtime = service.open_session(session_id)?;
            runtime.lock().unwrap().abort()?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Abort { session: snapshot })
        }
        Command::SetModel { session_id, model } => {
            let runtime = service.open_session(session_id)?;
            runtime.lock().unwrap().set_model(model)?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            Ok(CommandResult::SetModel { session: snapshot })
        }
        Command::SetThinking {
            session_id,
            thinking_level,
        } => {
            let runtime = service.open_session(session_id)?;
            runtime.lock().unwrap().set_thinking(thinking_level)?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            Ok(CommandResult::SetThinking { session: snapshot })
        }
    }
}

fn refresh_metadata(service: &mut dyn PiServerService, snapshots: &Arc<ServerSnapshotPublisher>) {
    if let Ok(sessions) = service.list_sessions() {
        snapshots.refresh(sessions);
    }
}
