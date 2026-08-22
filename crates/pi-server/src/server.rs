//! PiServer — port of `packages/server/src/server.ts`.

use std::sync::{Arc, Mutex};

use pi_protocol::{
    encode_server_message, is_supported_protocol_version, ClientMessage, Command, CommandResult,
    ProtocolError, ProtocolErrorCode, PROTOCOL_VERSION, ServerMessage,
};

use crate::connection::{
    is_terminal_connection, ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler,
    ConnectionState,
};
use crate::errors::PiServerError;
use crate::snapshots::ServerSnapshotPublisher;
use crate::types::{PiServerOptions, PiServerService};

/// Connection protocol handler.
pub struct ConnectionHandler {
    state: ConnectionState,
    service: Arc<Mutex<dyn PiServerService>>,
    snapshots: Arc<ServerSnapshotPublisher>,
    closing: Arc<Mutex<bool>>,
}


impl ConnectionHandler {
    fn receive(&mut self, chunk: &[u8], self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>) {
        if is_terminal_connection(&self.state) {
            return;
        }
        let messages = match self.state.decoder.push(chunk) {
            Ok(messages) => messages,
            Err(_) => {
                let _ = self
                    .fail_protocol_sync(ProtocolError {
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

    fn dispatch_message(&mut self, message: ClientMessage, self_arc: &Arc<Mutex<dyn ByteConnectionHandler>>) {
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
        // Shared with the async path: run the fail inline (close is async;
        // spawn it to avoid blocking on the socket).
        self.state.stage = "closing";
        if let Ok(frame) = encode_server_message(&ServerMessage::HelloError { error }, &Default::default()) {
            let connection = self.state.connection.clone();
            tokio::spawn(async move {
                let conn = connection;
                let _ = conn.close(Some(frame)).await;
            });
        }
        self.state.disconnected = true;
        self.state.stage = "closed";
        Ok(())
    }
}




/// Handshake future: pre-computes under lock, sends on the connection, then
/// applies post-send state under a fresh lock (the handler mutex is never
/// held across await).
async fn run_handshake(arc: Arc<Mutex<dyn ByteConnectionHandler>>, version: u64) {
    if !is_supported_protocol_version(version) {
        let frame = encode_server_message(
            &ServerMessage::HelloError {
                error: ProtocolError {
                    code: ProtocolErrorCode::Version,
                    message: format!("Unsupported protocol version {version}; expected {PROTOCOL_VERSION}"),
                    details: None,
                },
            },
            &Default::default(),
        )
        .ok();
        let (connection, frame) = {
            let mut guard = arc.lock().unwrap();
            let handler = guard.as_connection_handler().unwrap();
            handler.state.stage = "closing";
            handler.state.disconnected = true;
            handler.state.stage = "closed";
            (handler.state.connection.clone(), frame)
        };
        if let Some(frame) = frame {
            let conn = connection;
            let _ = conn.close(Some(frame)).await;
        }
        return;
    }
    let (connection, connection_id, snapshot) = {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        let snapshot = handler.snapshots.get();
        let connection = handler.state.connection.clone();
        let connection_id = handler.state.id.clone();
        (connection, connection_id, snapshot)
    };
    let message = ServerMessage::Hello { version: PROTOCOL_VERSION, connection_id, snapshot };
    let Ok(frame) = encode_server_message(&message, &Default::default()) else { return };
    let sent = {
        let conn = connection;
        conn.send(&frame).await.is_ok()
    };
    let mut guard = arc.lock().unwrap();
    let handler = guard.as_connection_handler().unwrap();
    if sent && !handler.state.disconnected && handler.state.stage == "handshaking" {
        handler.state.handshake_complete = true;
        handler.state.stage = "ready";
    }
}

/// Request future: executes the command with the service lock held briefly,
/// then sends the response through the connection (never holds the handler
/// mutex across awaits).
async fn run_request(arc: Arc<Mutex<dyn ByteConnectionHandler>>, id: String, request: Command) {
    let (connection, snapshots, closing, handshake_complete, service) = {
        let mut guard = arc.lock().unwrap();
        let handler = guard.as_connection_handler().unwrap();
        let connection = handler.state.connection.clone();
        let snapshots = handler.snapshots.clone();
        let closing = handler.closing.clone();
        let handshake_complete = handler.state.handshake_complete;
        let service = handler.service.clone();
        (connection, snapshots, closing, handshake_complete, service)
    };
    let result = {
        let mut service_guard = service.lock().unwrap();
        let _state_for_cmd = ConnectionStateStub;
        let snapshots_for_cmd = snapshots.clone();
        let outcome = run_command_sync(&mut *service_guard, request, &snapshots_for_cmd);
        outcome
    };
    let message = match &result {
        Ok(result) => ServerMessage::Response { id, ok: true, result: Some(result.clone()), error: None },
        Err(error) => ServerMessage::Response { id, ok: false, result: None, error: Some(error.into_protocol()) },
    };
    let Ok(frame) = encode_server_message(&message, &Default::default()) else { return };
    let conn = connection;
    let _ = conn.send(&frame).await;
    if !closing.lock().unwrap().clone() && handshake_complete {
        let snapshots_for_spawn = snapshots.clone();
        // Broadcast the full server snapshot AND a per-session snapshot event
        // for the mutated session (upstream publishes both after commands).
        let session_event = result.as_ref().ok().and_then(session_snapshot_of);
        tokio::spawn(async move {
            snapshots_for_spawn.broadcast().await;
            if let Some(session) = session_event {
                snapshots_for_spawn.broadcast_session_event(session).await;
            }
        });
    }
}

/// Sync command executor for the request path (the in-memory service trait is
/// synchronous; per-connection session-id registration is handled by the
/// run_command wrapper).
struct ConnectionStateStub;
/// PiServer: owns listeners, connections, snapshot publisher, service.
pub struct PiServer {
    pub id: String,
    service: Arc<Mutex<dyn PiServerService>>,
    connections: Arc<Mutex<Vec<Arc<Mutex<dyn ByteConnectionHandler>>>>>,
    snapshots: Arc<ServerSnapshotPublisher>,
    listeners: Vec<Box<dyn crate::listener::PiServerListener>>,
    closing: Arc<Mutex<bool>>,
}

impl PiServer {
    pub fn new(service: Box<dyn PiServerService>, options: PiServerOptions) -> Result<Self, String> {
        let id = options.server_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Prime the model cache synchronously (the in-memory service is
        // immediately available; heavier services populate via refresh).
        let service_box = service;
        let models = service_box.list_models().map_err(|e| e.to_string())?;
        let snapshots = Arc::new(ServerSnapshotPublisher::new(
            id.clone(),
            PROTOCOL_VERSION,
            models,
        ));
        let service: Arc<Mutex<dyn PiServerService>> = Arc::new(Mutex::new(service_box));
        Ok(Self {
            id,
            service,
            connections: Arc::new(Mutex::new(Vec::new())),
            snapshots,
            listeners: options.listeners,
            closing: Arc::new(Mutex::new(false)),
        })
    }

    pub fn snapshot_publisher(&self) -> Arc<ServerSnapshotPublisher> {
        self.snapshots.clone()
    }

    pub async fn start(&mut self) -> Result<(), String> {
        if *self.closing.lock().unwrap() {
            return Err("PiServer is closing or closed".to_string());
        }
        let accept = self.make_acceptor();
        for listener in &mut self.listeners {
            listener.start(accept.clone()).await?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<(), String> {
        *self.closing.lock().unwrap() = true;
        for listener in &mut self.listeners {
            let _ = listener.close().await;
        }
        self.connections.lock().unwrap().clear();
        Ok(())
    }

    pub fn addresses(&self) -> Vec<String> {
        self.listeners.iter().filter_map(|l| l.address()).collect()
    }

    fn make_acceptor(&self) -> ByteConnectionAcceptor {
        let connections = self.connections.clone();
        let snapshots = self.snapshots.clone();
        let service = self.service.clone();
        let closing = self.closing.clone();
        Arc::new(move |connection: Arc<dyn ByteConnection>| {
            let decoder = pi_protocol::ClientMessageDecoder::new(&Default::default())
                .expect("client message decoder");
            let state = ConnectionState::new(
                uuid::Uuid::new_v4().to_string(),
                connection.clone(),
                decoder,
            );
            snapshots.register_connection(connection);
            let handler = Arc::new(Mutex::new(ConnectionHandler {
                state,
                service: service.clone(),
                snapshots: snapshots.clone(),
                closing: closing.clone(),
            }));
            connections.lock().unwrap().push(handler.clone());
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
        self.state.disconnected = true;
        self.state.stage = "closed";
        self.snapshots.revoke_connection(&self.state.id);
        if !self.closing.lock().unwrap().clone() && self.state.handshake_complete {
            let snapshots_for_spawn = self.snapshots.clone();
            tokio::spawn(async move {
                snapshots_for_spawn.broadcast().await;
            });
        }
    }
    fn on_error(&mut self, _error: String) {}
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

/// Execute a protocol Command against the service; returns the protocol
/// result or a PiServerError.
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
        Command::Create { cwd, name, model, thinking_level } => {
            let id = uuid::Uuid::new_v4().to_string();
            let runtime = service
                .create_session(crate::types::CreateSessionOptions { id, cwd, name, model, thinking_level })?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Create { session: snapshot })
        }
        Command::Attach { session_id } => {
            let runtime = service.open_session(session_id.clone())?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            Ok(CommandResult::Attach { session: snapshot })
        }
        Command::Detach { session_id } => {
            Ok(CommandResult::Detach { session_id })
        }
        Command::Prompt { session_id, text } => {
            let runtime = service.open_session(session_id.clone())?;
            runtime.lock().unwrap().prompt(crate::types::PromptInput { text })?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Prompt { session: snapshot })
        }
        Command::Steer { session_id, text } => {
            let runtime = service.open_session(session_id.clone())?;
            runtime.lock().unwrap().steer(crate::types::SteerInput { text })?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Steer { session: snapshot })
        }
        Command::Abort { session_id } => {
            let runtime = service.open_session(session_id.clone())?;
            runtime.lock().unwrap().abort()?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            refresh_metadata(service, snapshots);
            Ok(CommandResult::Abort { session: snapshot })
        }
        Command::SetModel { session_id, model } => {
            let runtime = service.open_session(session_id.clone())?;
            runtime.lock().unwrap().set_model(model)?;
            let snapshot = runtime.lock().unwrap().snapshot()?;
            Ok(CommandResult::SetModel { session: snapshot })
        }
        Command::SetThinking { session_id, thinking_level } => {
            let runtime = service.open_session(session_id.clone())?;
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
