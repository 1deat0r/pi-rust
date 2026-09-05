//! Explicitly gated experimental CLI features.
//!
//! The upstream command parser exposes `server` and `client` as experimental
//! commands. This module keeps their parser and process lifecycle together so
//! an ordinary `pi` invocation cannot accidentally enter that surface.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_protocol::{
    AssistantContent, AssistantStatus, AssistantStopReason, AssistantTranscriptItem, Command,
    CommandResult, ModelMetadata, ModelRef, ProtocolErrorCode, SessionMetadata, SessionPhase,
    SessionSnapshot, TextContent, ThinkingContent, ThinkingLevel, ToolCallContent,
    TranscriptDeltaKind, TranscriptItem, TranscriptItemFinished, TranscriptItemUpdate,
    TranscriptProgress, UserContent as ProtocolUserContent, UserTranscriptItem,
};
use pi_server::{
    PiServer, PiServerError, PiServerOptions, PiServerService, PiSessionRuntime, UnixListener,
};
use url::Url;

use crate::config;
use crate::core::model_config::ModelConfig;
use crate::core::model_registry::ModelRegistry;
use crate::core::model_runtime::{
    refresh_provider_oauth_if_needed, register_faux_provider, resolve_run_model_for_provider,
    ModelRuntime,
};
use pi_agent::fs::StdFileSystem;
use pi_agent::harness::{AgentHarness, AgentHarnessOptions};
use pi_agent::session::types::{Entry, SessionMetadata as AgentSessionMetadata};
use pi_agent::session::{CreateOptions, JsonlSessionRepo};
use pi_agent::types::AgentMessage;
use pi_ai::model::Model;
use pi_ai::types::{
    AssistantMessage, ContentBlock, Message, ModelThinkingLevel, ThinkingLevel as AiThinkingLevel,
    UserContent,
};

const EXPERIMENTAL_GATE_ERROR: &str =
    "Experimental server/client commands require PI_EXPERIMENTAL=1";

pub fn are_enabled() -> bool {
    matches!(std::env::var("PI_EXPERIMENTAL").ok().as_deref(), Some("1"))
}

/// The upstream strict-tool preference used by built-in tools in experimental
/// mode. It is a preference (not a hard requirement).
pub fn experimental_tool_sampling() -> Option<pi_ai::types::ConstrainedSampling> {
    are_enabled().then_some(pi_ai::types::ConstrainedSampling::JsonSchema {
        strict: pi_ai::types::StrictPreference::Prefer,
    })
}

pub fn should_run_first_time_setup(
    official_distribution: bool,
    uses_default_agent_dir: bool,
    settings_path_exists: bool,
) -> bool {
    official_distribution && are_enabled() && uses_default_agent_dir && !settings_path_exists
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExperimentalCommand {
    Server {
        listen: Option<PathBuf>,
        auth: Option<AuthInput>,
    },
    Client {
        connect: Option<PathBuf>,
        auth: Option<AuthInput>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthInput {
    Token(String),
    File(PathBuf),
}

/// Parse only when the first argument selects an experimental subcommand.
/// Other invocations remain on the existing CLI parser path.
pub fn parse_command(argv: &[String]) -> Result<Option<ExperimentalCommand>, Vec<String>> {
    let Some(command) = argv.first().map(String::as_str) else {
        return Ok(None);
    };
    let is_server = command == "server";
    let is_client = command == "client";
    if !is_server && !is_client {
        return Ok(None);
    }
    if !are_enabled() {
        return Err(vec![EXPERIMENTAL_GATE_ERROR.to_string()]);
    }

    let mut listen = None;
    let mut connect = None;
    let mut auth = None;
    let mut auth_option = None;
    let mut errors = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--help" || argument == "-h" {
            return Err(vec![help_text(is_server)]);
        }
        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let takes_value = matches!(
            name,
            "--listen" | "--connect" | "--auth-token" | "--auth-token-file"
        );
        let value = if takes_value {
            match inline_value.or_else(|| {
                argv.get(index + 1)
                    .map(String::as_str)
                    .filter(|value| !value.starts_with('-'))
            }) {
                Some(value) if !value.is_empty() => {
                    if inline_value.is_none() {
                        index += 1;
                    }
                    Some(value)
                }
                _ => {
                    errors.push(format!("{name} requires a value"));
                    None
                }
            }
        } else {
            None
        };

        match name {
            "--listen" if is_server => {
                if let Some(value) = value {
                    if listen.is_some() {
                        errors.push("--listen may only be specified once".to_string());
                    } else if let Some(path) = parse_unix_address(value, "--listen", &mut errors) {
                        listen = Some(path);
                    }
                }
            }
            "--connect" if is_client => {
                if let Some(value) = value {
                    if connect.is_some() {
                        errors.push("--connect may only be specified once".to_string());
                    } else if let Some(path) = parse_unix_address(value, "--connect", &mut errors) {
                        connect = Some(path);
                    }
                }
            }
            "--auth-token" | "--auth-token-file" => {
                if let Some(value) = value {
                    if auth.is_some() {
                        if auth_option == Some(name) {
                            errors.push(format!("{name} may only be specified once"));
                        } else {
                            errors.push(
                                "--auth-token and --auth-token-file are mutually exclusive"
                                    .to_string(),
                            );
                        }
                    } else {
                        auth_option = Some(name);
                        auth = Some(if name == "--auth-token" {
                            AuthInput::Token(value.to_string())
                        } else {
                            AuthInput::File(PathBuf::from(config::expand_tilde_path(value)))
                        });
                    }
                }
            }
            _ => {
                errors.push(format!(
                    "The experimental {command} command does not support existing CLI options yet"
                ));
                break;
            }
        }
        index += 1;
    }

    if errors.is_empty() {
        Ok(Some(if is_server {
            ExperimentalCommand::Server { listen, auth }
        } else {
            ExperimentalCommand::Client { connect, auth }
        }))
    } else {
        Err(errors)
    }
}

fn parse_unix_address(value: &str, option: &str, errors: &mut Vec<String>) -> Option<PathBuf> {
    let Ok(url) = Url::parse(value) else {
        errors.push(format!("Invalid {option} address {value:?}"));
        return None;
    };
    if url.scheme() != "unix" {
        errors.push(format!("Unsupported {option} transport {:?}", url.scheme()));
        return None;
    }
    if url.host_str().is_some()
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        errors.push("Unix transport address must not include an authority".to_string());
        return None;
    }
    if !value.starts_with("unix:///")
        || value.starts_with("unix:////")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        errors.push(format!("Invalid {option} address {value:?}"));
        return None;
    }
    let Some(decoded_path) = percent_decode(url.path()) else {
        errors.push(format!("Invalid {option} address {value:?}"));
        return None;
    };
    let path = PathBuf::from(decoded_path);
    if !path.is_absolute() {
        errors.push("Unix transport address requires an absolute path".to_string());
        return None;
    }
    if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
        errors.push(format!("Invalid {option} address {value:?}"));
        return None;
    }
    Some(path)
}

fn percent_decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return None;
        }
        let high = (bytes[index + 1] as char).to_digit(16)? as u8;
        let low = (bytes[index + 2] as char).to_digit(16)? as u8;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

pub fn help_text(server: bool) -> String {
    if server {
        "Experimental server mode (requires PI_EXPERIMENTAL=1)\n\nUsage: pi server [--listen unix:///absolute/path]\n\nThe Rust server exposes the Pi protocol session lifecycle.\n".to_string()
    } else {
        "Experimental client mode (requires PI_EXPERIMENTAL=1)\n\nUsage: pi client --connect unix:///absolute/path\n\nThe client connects, handshakes, lists sessions, and exits.\n".to_string()
    }
}

fn resolve_auth_input(auth: Option<AuthInput>) -> Result<Option<String>, String> {
    match auth {
        None => Ok(None),
        Some(AuthInput::Token(token)) if !token.is_empty() && !token.contains(['\r', '\n']) => {
            Ok(Some(token))
        }
        Some(AuthInput::Token(_)) => {
            Err("auth token must be non-empty and contain no newlines".to_string())
        }
        Some(AuthInput::File(path)) => {
            let token = std::fs::read_to_string(&path)
                .map_err(|error| format!("read auth token file {}: {error}", path.display()))?
                .trim_end_matches(['\r', '\n'])
                .to_string();
            if token.is_empty() || token.contains(['\r', '\n']) {
                return Err(format!(
                    "auth token file {} contains an invalid token",
                    path.display()
                ));
            }
            Ok(Some(token))
        }
    }
}

pub async fn run_server(command: ExperimentalCommand) -> Result<(), String> {
    let ExperimentalCommand::Server { listen, auth } = command else {
        return Err("internal error: expected experimental server command".to_string());
    };
    let auth_token = resolve_auth_input(auth)?;
    let socket = listen.unwrap_or_else(default_server_socket);
    let listener = UnixListener::new(socket.to_string_lossy().into_owned())
        .map_err(|error| format!("invalid server listener: {error}"))?;
    let listener = match auth_token {
        Some(token) => listener
            .with_auth_token(token)
            .map_err(|error| format!("invalid server authentication: {error}"))?,
        None => listener,
    };
    let mut server = PiServer::new(
        Box::new(
            CliSessionService::load()
                .map_err(|error| format!("load experimental server runtime: {error}"))?,
        ),
        PiServerOptions {
            listeners: vec![Box::new(listener)],
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: None,
            on_error: None,
        },
    )
    .map_err(|error| format!("create experimental server: {error}"))?;
    server
        .start()
        .await
        .map_err(|error| format!("start experimental server: {error}"))?;
    let address = server
        .addresses()
        .first()
        .cloned()
        .unwrap_or_else(|| socket.to_string_lossy().into_owned());
    println!("Experimental server listening on unix://{address}");
    let signal = wait_for_server_shutdown_signal().await;
    let close_result = server.close().await;
    if let Err(error) = signal {
        return Err(format!("wait for server shutdown signal: {error}"));
    }
    close_result.map_err(|error| format!("close experimental server: {error}"))
}

#[cfg(unix)]
async fn wait_for_server_shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = sigterm.recv() => Ok(()),
        _ = sighup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn wait_for_server_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

pub async fn run_client(command: ExperimentalCommand) -> Result<(), String> {
    let ExperimentalCommand::Client { connect, auth } = command else {
        return Err("internal error: expected experimental client command".to_string());
    };
    let auth_token = resolve_auth_input(auth)?;
    let socket = connect
        .or_else(|| std::env::var("PI_SERVER_SOCKET").ok().map(PathBuf::from))
        .ok_or_else(|| "--connect is required (or set PI_SERVER_SOCKET)".to_string())?;
    let socket_string = socket.to_string_lossy().into_owned();
    if let Some(token) = auth_token {
        return run_authenticated_client(&socket_string, &token).await;
    }
    let client = pi_client::PiClient::connect_with_timeouts(
        &socket_string,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(5),
    )
    .await
    .map_err(|error| format!("connect experimental server: {error}"))?;
    let snapshot = client
        .snapshot()
        .ok_or_else(|| "experimental client connected without a server snapshot".to_string())?;
    let result = client
        .request(Command::List)
        .await
        .map_err(|error| format!("list experimental server sessions: {error}"));
    let close_result = client.close().await;
    let dispose_result = client.dispose().await;
    let result = result?;
    close_result.map_err(|error| format!("close experimental client: {error}"))?;
    dispose_result.map_err(|error| format!("dispose experimental client: {error}"))?;
    let session_count = match result {
        CommandResult::List { sessions } => sessions.len(),
        _ => return Err("experimental client received an invalid list response".to_string()),
    };
    println!(
        "Connected to experimental server {} ({} session{})",
        snapshot.server_id,
        session_count,
        if session_count == 1 { "" } else { "s" }
    );
    Ok(())
}

async fn run_authenticated_client(socket: &str, token: &str) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|error| format!("connect experimental server: {error}"))?;
    stream
        .write_all(format!("PI-AUTH {token}\n").as_bytes())
        .await
        .map_err(|error| format!("send experimental server auth: {error}"))?;
    let options = pi_protocol::FrameDecoderOptions {
        max_frame_length: None,
    };
    let hello = pi_protocol::encode_client_message(
        &pi_protocol::ClientMessage::Hello {
            version: pi_protocol::PROTOCOL_VERSION,
        },
        &options,
    )
    .map_err(|error| format!("encode experimental client hello: {error}"))?;
    stream
        .write_all(&hello)
        .await
        .map_err(|error| format!("send experimental client hello: {error}"))?;
    let mut decoder = pi_protocol::ServerMessageDecoder::new(&options)
        .map_err(|error| format!("create experimental client decoder: {error}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let snapshot = 'handshake: loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .map_err(|_| "experimental client handshake timed out".to_string())?
            .map_err(|error| format!("read experimental server handshake: {error}"))?;
        if n == 0 {
            return Err("experimental server closed during authenticated handshake".into());
        }
        for message in decoder
            .push(&buf[..n])
            .map_err(|error| format!("decode experimental server handshake: {error}"))?
        {
            match message {
                pi_protocol::ServerMessage::Hello { snapshot, .. } => break 'handshake snapshot,
                pi_protocol::ServerMessage::HelloError { error } => {
                    return Err(format!(
                        "experimental server rejected hello: {}",
                        error.message
                    ));
                }
                _ => {}
            }
        }
    };
    let request = pi_protocol::encode_client_message(
        &pi_protocol::ClientMessage::Request {
            id: "r1".into(),
            request: Command::List,
        },
        &options,
    )
    .map_err(|error| format!("encode experimental client list: {error}"))?;
    stream
        .write_all(&request)
        .await
        .map_err(|error| format!("send experimental client list: {error}"))?;
    let session_count = 'list: loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .map_err(|_| "experimental client list timed out".to_string())?
            .map_err(|error| format!("read experimental server list: {error}"))?;
        if n == 0 {
            return Err("experimental server closed before list response".into());
        }
        for message in decoder
            .push(&buf[..n])
            .map_err(|error| format!("decode experimental server list: {error}"))?
        {
            if let pi_protocol::ServerMessage::Response {
                result: Some(CommandResult::List { sessions }),
                ..
            } = message
            {
                break 'list sessions.len();
            }
        }
    };
    println!(
        "Connected to experimental server {} ({} session{})",
        snapshot.server_id,
        session_count,
        if session_count == 1 { "" } else { "s" }
    );
    Ok(())
}

fn default_server_socket() -> PathBuf {
    config::get_agent_dir().join("server.sock")
}

#[derive(Clone)]
struct CliRuntimeConfig {
    models: pi_ai::models::Models,
    model_runtime: ModelRuntime,
    model: Model,
    session_root: PathBuf,
}

struct CliSessionBackend {
    harness: Arc<AgentHarness<StdFileSystem>>,
    snapshot: Arc<Mutex<SessionSnapshot>>,
    listeners: Arc<Mutex<Vec<Option<pi_server::service::EventListener>>>>,
    pending: Arc<Mutex<Option<pi_server::service::RuntimeWait>>>,
    config: CliRuntimeConfig,
}

#[derive(Clone)]
struct CliCreateOptions {
    name: Option<String>,
    cwd: Option<String>,
    model: Option<ModelRef>,
    thinking_level: Option<ThinkingLevel>,
}

#[derive(Clone)]
struct CliSessionService {
    sessions: Arc<Mutex<BTreeMap<String, Arc<CliSessionBackend>>>>,
    config: CliRuntimeConfig,
}

impl CliSessionService {
    fn load() -> Result<Self, String> {
        let config = build_runtime_config()?;
        let sessions = Arc::new(Mutex::new(BTreeMap::new()));
        let root = config.session_root.clone();
        let metadata = blocking(async move {
            let fs = StdFileSystem::new(config::cwd());
            JsonlSessionRepo::new(fs, root.to_string_lossy())
                .list(None)
                .await
        })?;
        for metadata in metadata {
            let backend = build_backend(config.clone(), None, metadata)?;
            install_agent_listener(&backend);
            sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(backend.id(), backend);
        }
        Ok(Self { sessions, config })
    }
}

impl PiServerService for CliSessionService {
    fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiServerError> {
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(|backend| {
                let snapshot = backend
                    .snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                SessionMetadata {
                    id: snapshot.id.clone(),
                    created_at: snapshot.created_at,
                    updated_at: Some(snapshot.updated_at),
                    parent_session_id: None,
                    session_name: snapshot.name.clone(),
                    cwd: Some(snapshot.cwd.clone()),
                }
            })
            .collect())
    }

    fn list_models(&self) -> Result<Vec<ModelMetadata>, PiServerError> {
        Ok(vec![model_metadata(
            &self.config.model,
            &self.config.models,
        )])
    }

    fn create_session(
        &mut self,
        options: pi_server::CreateSessionOptions,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sessions.contains_key(&options.id) {
            return Err(PiServerError::new(
                ProtocolErrorCode::SessionLocked,
                "Session already exists",
            ));
        }
        let create_options = CliCreateOptions {
            name: options.name.clone(),
            cwd: options.cwd.clone(),
            model: options.model.clone(),
            thinking_level: options.thinking_level.clone(),
        };
        let backend = build_backend(
            self.config.clone(),
            Some(create_options),
            AgentSessionMetadata {
                id: options.id.clone(),
                created_at: pi_ai::types::now_ms(),
                cwd: options.cwd.clone().unwrap_or_else(config::cwd),
                path: String::new(),
                modified_at: pi_ai::types::now_ms(),
                source_format: 4,
                parent_session_id: None,
                legacy_parent_session_path: None,
                metadata: None,
            },
        )
        .map_err(runtime_error)?;
        install_agent_listener(&backend);
        let runtime = Arc::new(Mutex::new(CliSessionRuntime {
            backend: backend.clone(),
        }));
        sessions.insert(options.id, backend);
        Ok(runtime)
    }

    fn open_session(
        &mut self,
        session_id: String,
    ) -> Result<Arc<Mutex<dyn PiSessionRuntime>>, PiServerError> {
        let backend = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&session_id)
            .cloned()
            .ok_or_else(|| PiServerError::new(ProtocolErrorCode::NotFound, "Session not found"))?;
        Ok(Arc::new(Mutex::new(CliSessionRuntime { backend })))
    }
}

struct CliSessionRuntime {
    backend: Arc<CliSessionBackend>,
}

impl PiSessionRuntime for CliSessionRuntime {
    fn snapshot(&mut self) -> Result<SessionSnapshot, PiServerError> {
        Ok(self
            .backend
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone())
    }

    fn get_phase(&self) -> SessionPhase {
        self.backend
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            .clone()
    }

    fn prompt(&mut self, input: pi_server::PromptInput) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                ProtocolErrorCode::Busy,
                "Session is busy",
            ));
        }
        self.backend.update_snapshot(|snapshot| {
            snapshot.phase = SessionPhase::Turn;
        });
        let backend = self.backend.clone();
        let text = input.text;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| runtime_error("prompt requires a Tokio runtime"))?;
        let task = handle.spawn(async move {
            if backend.config.model.provider != "faux" {
                refresh_provider_oauth_if_needed(
                    &backend.config.models,
                    &backend.config.model.provider,
                )
                .await
                .map_err(runtime_error)?;
            }
            let lane = backend.harness.lane("main").await.map_err(runtime_error)?;
            lane.prompt_text(&text, &[]).await.map_err(runtime_error)?;
            backend.refresh_snapshot().await.map_err(runtime_error)
        });
        *self
            .backend
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(pi_server::service::RuntimeWait::new(async move {
                task.await
                    .map_err(|error| runtime_error(error.to_string()))?
            }));
        Ok(())
    }

    fn steer(&mut self, input: pi_server::SteerInput) -> Result<(), PiServerError> {
        if self.get_phase() == SessionPhase::Idle {
            return Err(PiServerError::new(
                ProtocolErrorCode::Busy,
                "There is no active prompt",
            ));
        }
        let backend = self.backend.clone();
        let text = input.text;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| runtime_error("steer requires a Tokio runtime"))?;
        handle.spawn(async move {
            loop {
                if backend
                    .harness
                    .agent_handle()
                    .is_some_and(|agent| agent.is_streaming())
                {
                    let lane = backend.harness.lane("main").await.map_err(runtime_error)?;
                    lane.steer_text(&text, &[]).await.map_err(runtime_error)?;
                    backend.update_snapshot(|snapshot| snapshot.queued_steer_count += 1);
                    break;
                }
                if backend.get_phase() == SessionPhase::Idle {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<(), PiServerError>(())
        });
        Ok(())
    }

    fn abort(&mut self) -> Result<(), PiServerError> {
        if self.get_phase() == SessionPhase::Idle {
            return Err(PiServerError::new(
                ProtocolErrorCode::Busy,
                "There is no active prompt",
            ));
        }
        if let Some(agent) = self.backend.harness.agent_handle() {
            agent.abort();
        }
        let backend = self.backend.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Ok(lane) = backend.harness.lane("main").await {
                    let _ = lane.abort().await;
                }
            });
        }
        Ok(())
    }

    fn take_pending_operation(&mut self) -> Option<pi_server::service::RuntimeWait> {
        self.backend
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn set_model(&mut self, model: ModelRef) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                ProtocolErrorCode::Busy,
                "Session is busy",
            ));
        }
        let selected = self
            .backend
            .config
            .models
            .get_models(Some(&model.provider))
            .into_iter()
            .find(|candidate| candidate.id == model.id)
            .ok_or_else(|| {
                PiServerError::new(ProtocolErrorCode::InvalidRequest, "Unknown model")
            })?;
        if let Some(agent) = self.backend.harness.agent_handle() {
            agent.state().model = selected;
        }
        self.backend
            .update_snapshot(|snapshot| snapshot.model = model);
        Ok(())
    }

    fn set_thinking(&mut self, thinking_level: ThinkingLevel) -> Result<(), PiServerError> {
        if self.get_phase() != SessionPhase::Idle {
            return Err(PiServerError::new(
                ProtocolErrorCode::Busy,
                "Session is busy",
            ));
        }
        if let Some(agent) = self.backend.harness.agent_handle() {
            agent.state().thinking_level = to_agent_thinking_level(thinking_level.clone());
        }
        self.backend
            .update_snapshot(|snapshot| snapshot.thinking_level = thinking_level);
        Ok(())
    }

    fn subscribe(
        &mut self,
        listener: pi_server::service::EventListener,
    ) -> Result<pi_server::service::Unsubscribe, PiServerError> {
        let listeners = self.backend.listeners.clone();
        let mut slots = listeners.lock().unwrap_or_else(|error| error.into_inner());
        slots.push(Some(listener));
        let index = slots.len() - 1;
        drop(slots);
        Ok(Box::new(move || {
            if let Some(slot) = listeners
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_mut(index)
            {
                *slot = None;
            }
        }))
    }

    fn dispose(&mut self) -> Result<(), PiServerError> {
        self.backend
            .listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        Ok(())
    }
}

fn runtime_error(error: impl std::fmt::Display) -> PiServerError {
    PiServerError::new(ProtocolErrorCode::InternalError, error.to_string())
}

fn blocking<T, E>(
    future: impl std::future::Future<Output = Result<T, E>> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?
            .block_on(future)
            .map_err(|error| error.to_string())
    })
    .join()
    .map_err(|_| "runtime worker thread panicked".to_string())?
}

fn build_runtime_config() -> Result<CliRuntimeConfig, String> {
    let provider = config::resolve_provider(None);
    let (models, model) = if provider == "faux" {
        let models = pi_ai::models::create_models(Default::default());
        let core = register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions {
                tokens_per_second: Some(40.0),
                ..Default::default()
            },
        );
        let responses = (0..256)
            .map(|_| {
                pi_ai::providers::FauxResponseStep::Message(
                    pi_ai::providers::faux_assistant_message(
                        vec![ContentBlock::text("experimental faux provider response")],
                        Default::default(),
                    ),
                )
            })
            .collect();
        core.set_responses(responses);
        let model = core
            .models
            .first()
            .cloned()
            .ok_or_else(|| "faux provider has no models".to_string())?;
        (models, model)
    } else {
        let base = crate::core::model_registry::builtin_models();
        let registry = ModelRegistry::new(
            base,
            ModelConfig::load(crate::core::model_config::models_json_path().as_deref()),
        );
        let models = registry.into_models();
        if models.get_provider(&provider).is_none() {
            return Err(format!(
                "provider {provider:?} is not registered in the model registry"
            ));
        }
        let model = resolve_run_model_for_provider(
            &models,
            &provider,
            config::resolve_model(None).as_deref(),
        )?;
        (models, model)
    };
    let model_runtime = ModelRuntime::new(models.clone());
    Ok(CliRuntimeConfig {
        models,
        model_runtime,
        model,
        session_root: config::get_session_dir(),
    })
}

fn selected_model(
    config: &CliRuntimeConfig,
    requested: Option<&ModelRef>,
) -> Result<Model, String> {
    match requested {
        Some(reference) => config
            .models
            .get_models(Some(&reference.provider))
            .into_iter()
            .find(|model| model.id == reference.id)
            .ok_or_else(|| format!("unknown model {}/{}", reference.provider, reference.id)),
        None => Ok(config.model.clone()),
    }
}

fn to_agent_thinking_level(level: ThinkingLevel) -> Option<AiThinkingLevel> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(AiThinkingLevel::Minimal),
        ThinkingLevel::Low => Some(AiThinkingLevel::Low),
        ThinkingLevel::Medium => Some(AiThinkingLevel::Medium),
        ThinkingLevel::High => Some(AiThinkingLevel::High),
        ThinkingLevel::Xhigh => Some(AiThinkingLevel::Xhigh),
        ThinkingLevel::Max => Some(AiThinkingLevel::Max),
    }
}

fn to_model_thinking_level(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Off => ModelThinkingLevel::Off,
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn build_backend(
    config: CliRuntimeConfig,
    create: Option<CliCreateOptions>,
    metadata: AgentSessionMetadata,
) -> Result<Arc<CliSessionBackend>, String> {
    let cwd = create
        .as_ref()
        .and_then(|options| options.cwd.clone())
        .unwrap_or_else(|| metadata.cwd.clone());
    let requested_model = create.as_ref().and_then(|options| options.model.clone());
    let model = selected_model(&config, requested_model.as_ref())?;
    let thinking_level = create
        .as_ref()
        .and_then(|options| options.thinking_level.clone())
        .unwrap_or(ThinkingLevel::Off);
    let id = metadata.id.clone();
    let session_root = config.session_root.clone();
    let stream_runtime = config.model_runtime.clone();
    let stream_options = pi_ai::types::StreamOptions {
        base: pi_ai::types::ProviderRequestOptions {
            api_key: config::env(config::ENV_KEY),
            ..Default::default()
        },
        ..Default::default()
    };
    let stream_fn: pi_agent::agent::StreamFn = Arc::new(move |stream_model, context| {
        stream_runtime.stream(stream_model, context, Some(&stream_options))
    });
    let with_options_runtime = config.model_runtime.clone();
    let stream_fn_with_options: pi_agent::agent::StreamFnWithOptions =
        Arc::new(move |stream_model, context, turn_options| {
            let mut merged = pi_ai::types::SimpleStreamOptions {
                base: pi_ai::types::StreamOptions {
                    base: pi_ai::types::ProviderRequestOptions {
                        api_key: config::env(config::ENV_KEY),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            if turn_options.reasoning.is_some() {
                merged.reasoning = turn_options.reasoning;
            }
            with_options_runtime.stream_simple(stream_model, context, Some(&merged))
        });
    let create_session = create.is_some();
    let async_cwd = cwd.clone();
    let async_id = id.clone();
    let async_metadata = metadata.clone();
    let async_model = model.clone();
    let async_thinking_level = thinking_level.clone();
    let async_stream_fn = stream_fn;
    let async_stream_fn_with_options = stream_fn_with_options;
    let (harness, transcript) = blocking(async move {
        let fs = StdFileSystem::new(&async_cwd);
        let session = if create_session {
            JsonlSessionRepo::new(fs, session_root.to_string_lossy())
                .create(CreateOptions::new(async_cwd.clone()).with_id(async_id))
                .await
                .map_err(|error| error.to_string())?
        } else {
            JsonlSessionRepo::new(fs, session_root.to_string_lossy())
                .open(&async_metadata)
                .await
                .map_err(|error| error.to_string())?
        };
        let mut options = AgentHarnessOptions::new(session, async_model);
        options.stream_fn = Some(async_stream_fn);
        options.stream_fn_with_options = Some(async_stream_fn_with_options);
        options.thinking_level = Some(to_model_thinking_level(async_thinking_level));
        let (harness, _) = AgentHarness::create(options)
            .await
            .map_err(|error| error.to_string())?;
        let transcript = harness
            .transcript()
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((harness, transcript))
    })?;
    let now = pi_ai::types::now_ms() as i64;
    let snapshot = SessionSnapshot {
        id: metadata.id,
        name: create.as_ref().and_then(|options| options.name.clone()),
        cwd,
        created_at: metadata.created_at as i64,
        updated_at: metadata.modified_at.max(metadata.created_at) as i64,
        phase: SessionPhase::Idle,
        model: ModelRef {
            provider: model.provider.clone(),
            id: model.id.clone(),
        },
        thinking_level,
        attached: false,
        locked: false,
        revision: transcript.len() as i64,
        transcript: transcript_items(&transcript, &model),
        queued_steer: Vec::new(),
        queued_steer_count: 0,
    };
    Ok(Arc::new(CliSessionBackend {
        harness: Arc::new(harness),
        snapshot: Arc::new(Mutex::new(SessionSnapshot {
            updated_at: now,
            ..snapshot
        })),
        listeners: Arc::new(Mutex::new(Vec::new())),
        pending: Arc::new(Mutex::new(None)),
        config,
    }))
}

impl CliSessionBackend {
    fn id(&self) -> String {
        self.snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .id
            .clone()
    }

    fn get_phase(&self) -> SessionPhase {
        self.snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
            .clone()
    }

    fn update_snapshot(&self, update: impl FnOnce(&mut SessionSnapshot)) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update(&mut snapshot);
        snapshot.revision += 1;
        snapshot.updated_at = pi_ai::types::now_ms() as i64;
        drop(snapshot);
        self.emit(pi_server::PiSessionRuntimeEvent::Snapshot);
    }

    fn emit(&self, event: pi_server::PiSessionRuntimeEvent) {
        for listener in self
            .listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .into_iter()
            .flatten()
        {
            listener(event.clone());
        }
    }

    async fn refresh_snapshot(&self) -> Result<(), String> {
        let entries = self
            .harness
            .transcript()
            .await
            .map_err(|error| error.to_string())?;
        let model = self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .model
            .clone();
        let items = transcript_items(&entries, &self.config.model);
        {
            let mut snapshot = self
                .snapshot
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            snapshot.phase = SessionPhase::Idle;
            snapshot.transcript = items;
            snapshot.queued_steer.clear();
            snapshot.queued_steer_count = 0;
            snapshot.model = model;
            snapshot.revision += 1;
            snapshot.updated_at = pi_ai::types::now_ms() as i64;
        }
        self.emit(pi_server::PiSessionRuntimeEvent::Snapshot);
        Ok(())
    }
}

fn install_agent_listener(backend: &Arc<CliSessionBackend>) {
    let Some(agent) = backend.harness.agent_handle() else {
        return;
    };
    let weak = Arc::downgrade(backend);
    let _unsubscribe = agent.subscribe(move |event, _signal| {
        let backend = weak.clone();
        Box::pin(async move {
            let Some(backend) = backend.upgrade() else {
                return;
            };
            handle_agent_event(&backend, event);
        })
    });
}

fn handle_agent_event(backend: &CliSessionBackend, event: pi_agent::rich_agent::RichAgentEvent) {
    use pi_ai::types::AssistantMessageEvent;
    match event {
        pi_agent::rich_agent::RichAgentEvent::MessageStart { message } => {
            if let Some(assistant) = assistant_message(&message) {
                let item = assistant_item("streaming", &assistant, &backend.config.model);
                backend.emit(pi_server::PiSessionRuntimeEvent::Progress(
                    TranscriptProgress::ItemStarted {
                        item: TranscriptItem::Assistant(item),
                    },
                ));
            }
        }
        pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            if let Some(assistant) = assistant_message(&message) {
                let id = format!("streaming-{}", assistant.timestamp());
                let delta = match &assistant_message_event {
                    AssistantMessageEvent::TextDelta {
                        content_index,
                        delta,
                        ..
                    } => Some((
                        TranscriptDeltaKind::Text,
                        *content_index as i64,
                        delta.clone(),
                    )),
                    AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta,
                        ..
                    } => Some((
                        TranscriptDeltaKind::Thinking,
                        *content_index as i64,
                        delta.clone(),
                    )),
                    AssistantMessageEvent::ToolCallDelta {
                        content_index,
                        delta,
                        ..
                    } => Some((
                        TranscriptDeltaKind::ToolCall,
                        *content_index as i64,
                        delta.clone(),
                    )),
                    _ => None,
                };
                if let Some((kind, content_index, delta)) = delta {
                    backend.emit(pi_server::PiSessionRuntimeEvent::Progress(
                        TranscriptProgress::AssistantDelta {
                            message_id: id,
                            content_index,
                            kind,
                            delta,
                        },
                    ));
                }
                backend.emit(pi_server::PiSessionRuntimeEvent::Progress(
                    TranscriptProgress::ItemUpdated {
                        item: TranscriptItemUpdate::Assistant(assistant_item(
                            "streaming",
                            &assistant,
                            &backend.config.model,
                        )),
                    },
                ));
            }
        }
        pi_agent::rich_agent::RichAgentEvent::MessageEnd { message } => {
            if let Some(assistant) = assistant_message(&message) {
                let status = if assistant.stop_reason() == Some(pi_ai::types::StopReason::Aborted) {
                    "aborted"
                } else if assistant.stop_reason() == Some(pi_ai::types::StopReason::Error) {
                    "error"
                } else {
                    "complete"
                };
                let item = assistant_item(status, &assistant, &backend.config.model);
                backend.emit(pi_server::PiSessionRuntimeEvent::Progress(
                    TranscriptProgress::ItemFinished {
                        item: match status {
                            "aborted" => TranscriptItemFinished::AbortedAssistant(item),
                            "error" => TranscriptItemFinished::ErrorAssistant(item),
                            _ => TranscriptItemFinished::CompleteAssistant(item),
                        },
                    },
                ));
            }
        }
        _ => {}
    }
}

fn assistant_message(message: &AgentMessage) -> Option<AssistantMessage> {
    match message {
        AgentMessage::Core(Message::Assistant(message)) => Some(message.clone()),
        _ => None,
    }
}

fn protocol_user_content(content: &UserContent) -> Vec<ProtocolUserContent> {
    match content {
        UserContent::RoleUser { content, .. } => match content {
            pi_ai::types::UserContentBody::String(text) => {
                vec![ProtocolUserContent::Text(TextContent::Text {
                    text: text.clone(),
                })]
            }
            pi_ai::types::UserContentBody::Blocks(blocks) => {
                blocks.iter().filter_map(protocol_user_block).collect()
            }
        },
    }
}

fn protocol_user_block(block: &ContentBlock) -> Option<ProtocolUserContent> {
    match block {
        ContentBlock::Text { text, .. } => Some(ProtocolUserContent::Text(TextContent::Text {
            text: text.clone(),
        })),
        ContentBlock::Image { data, mime_type } => Some(ProtocolUserContent::Image(
            pi_protocol::ImageContent::Image {
                data: data.clone(),
                mime_type: mime_type.clone(),
            },
        )),
        _ => None,
    }
}

fn protocol_assistant_block(block: &ContentBlock) -> Option<AssistantContent> {
    match block {
        ContentBlock::Text { text, .. } => Some(AssistantContent::Text(TextContent::Text {
            text: text.clone(),
        })),
        ContentBlock::Thinking {
            thinking, redacted, ..
        } => Some(AssistantContent::Thinking(ThinkingContent::Thinking {
            thinking: thinking.clone(),
            redacted: *redacted,
        })),
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(AssistantContent::ToolCall(ToolCallContent::ToolCall {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            input: arguments.clone(),
        })),
        ContentBlock::Image { .. } => None,
    }
}

fn assistant_item(
    status: &str,
    message: &AssistantMessage,
    default_model: &Model,
) -> AssistantTranscriptItem {
    let status = match status {
        "streaming" => AssistantStatus::Streaming,
        "aborted" => AssistantStatus::Aborted,
        "error" => AssistantStatus::Error,
        _ => AssistantStatus::Complete,
    };
    // The wire schema deliberately omits terminal-only fields while an item
    // is streaming. Keeping a synthetic stop reason on the partial item makes
    // strict protocol validation reject every progress frame and closes the
    // server connection before the correlated prompt response can arrive.
    let stop_reason = if matches!(status, AssistantStatus::Streaming) {
        None
    } else {
        Some(match message.stop_reason() {
            Some(pi_ai::types::StopReason::Length) => AssistantStopReason::Length,
            Some(pi_ai::types::StopReason::ToolUse) => AssistantStopReason::ToolUse,
            Some(pi_ai::types::StopReason::Error) => AssistantStopReason::Error,
            Some(pi_ai::types::StopReason::Aborted) => AssistantStopReason::Aborted,
            _ => AssistantStopReason::Stop,
        })
    };
    let error_message = if matches!(status, AssistantStatus::Error | AssistantStatus::Aborted) {
        message.error_message().map(str::to_string)
    } else {
        None
    };
    AssistantTranscriptItem {
        id: format!("streaming-{}", message.timestamp()),
        role: "assistant".to_string(),
        content: message
            .content()
            .iter()
            .filter_map(protocol_assistant_block)
            .collect(),
        model: ModelRef {
            provider: message
                .provider()
                .unwrap_or(&default_model.provider)
                .to_string(),
            id: message.model().unwrap_or(&default_model.id).to_string(),
        },
        response_model: message.response_id().map(str::to_string),
        usage: message
            .usage()
            .and_then(|usage| serde_json::to_value(usage).ok())
            .and_then(|value| serde_json::from_value(value).ok()),
        timestamp: message.timestamp() as i64,
        status,
        stop_reason,
        error_message,
    }
}

fn transcript_items(entries: &[Entry], model: &Model) -> Vec<TranscriptItem> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Message {
                id,
                timestamp,
                message,
                ..
            } => match message {
                AgentMessage::Core(Message::User(user)) => {
                    Some(TranscriptItem::User(UserTranscriptItem {
                        id: id.clone(),
                        role: "user".to_string(),
                        content: protocol_user_content(user),
                        timestamp: *timestamp as i64,
                    }))
                }
                AgentMessage::Core(Message::Assistant(assistant)) => {
                    Some(TranscriptItem::Assistant({
                        let mut item = assistant_item("complete", assistant, model);
                        item.id = id.clone();
                        item
                    }))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn model_metadata(model: &Model, models: &pi_ai::models::Models) -> ModelMetadata {
    ModelMetadata {
        provider: model.provider.clone(),
        id: model.id.clone(),
        name: model.name.clone(),
        api: model.api.clone(),
        reasoning: model.reasoning,
        input: model
            .input
            .iter()
            .map(|input| match input {
                pi_ai::model::ModelInput::Text => pi_protocol::ModelInput::Text,
                pi_ai::model::ModelInput::Image => pi_protocol::ModelInput::Image,
            })
            .collect(),
        context_window: model.context_window as i64,
        max_tokens: model.max_tokens as i64,
        cost: pi_protocol::ModelCost {
            input: model.cost.input,
            output: model.cost.output,
            cache_read: model.cost.cache_read,
            cache_write: model.cost.cache_write,
        },
        supported_thinking_levels: pi_ai::model::get_supported_thinking_levels(model)
            .into_iter()
            .map(|level| match level {
                ModelThinkingLevel::Off => ThinkingLevel::Off,
                ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
                ModelThinkingLevel::Low => ThinkingLevel::Low,
                ModelThinkingLevel::Medium => ThinkingLevel::Medium,
                ModelThinkingLevel::High => ThinkingLevel::High,
                ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
                ModelThinkingLevel::Max => ThinkingLevel::Max,
            })
            .collect(),
        authenticated: models.check_auth(&model.provider).is_some(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use pi_protocol::{ServerEvent, ServerMessage};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn experimental_gate_requires_exact_one() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("PI_EXPERIMENTAL");
        std::env::remove_var("PI_EXPERIMENTAL");
        assert!(!are_enabled());
        assert!(experimental_tool_sampling().is_none());
        std::env::set_var("PI_EXPERIMENTAL", "true");
        assert!(!are_enabled());
        assert!(experimental_tool_sampling().is_none());
        std::env::set_var("PI_EXPERIMENTAL", "1");
        assert!(are_enabled());
        assert_eq!(
            experimental_tool_sampling(),
            Some(pi_ai::types::ConstrainedSampling::JsonSchema {
                strict: pi_ai::types::StrictPreference::Prefer,
            })
        );
        match previous {
            Some(value) => std::env::set_var("PI_EXPERIMENTAL", value),
            None => std::env::remove_var("PI_EXPERIMENTAL"),
        }
    }

    #[test]
    fn first_time_setup_requires_every_boundary() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("PI_EXPERIMENTAL");
        std::env::set_var("PI_EXPERIMENTAL", "1");
        assert!(should_run_first_time_setup(true, true, false));
        assert!(!should_run_first_time_setup(false, true, false));
        assert!(!should_run_first_time_setup(true, false, false));
        assert!(!should_run_first_time_setup(true, true, true));
        match previous {
            Some(value) => std::env::set_var("PI_EXPERIMENTAL", value),
            None => std::env::remove_var("PI_EXPERIMENTAL"),
        }
    }

    #[test]
    fn streaming_assistant_progress_is_wire_valid_without_terminal_fields() {
        let model = Model::new("faux-1", "Faux", "faux", "faux");
        let message = AssistantMessage::new().with_timestamp(1);
        let item = assistant_item("streaming", &message, &model);
        assert_eq!(item.status, AssistantStatus::Streaming);
        assert!(item.stop_reason.is_none());
        assert!(item.error_message.is_none());

        let message = ServerMessage::Event {
            event: ServerEvent::SessionProgress {
                session_id: "session-1".to_string(),
                progress: TranscriptProgress::ItemStarted {
                    item: TranscriptItem::Assistant(item),
                },
            },
        };
        assert!(pi_protocol::encode_server_message(
            &message,
            &pi_protocol::FrameDecoderOptions::default()
        )
        .is_ok());
    }

    #[test]
    fn command_parser_is_gated_and_validates_unix_transport() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("PI_EXPERIMENTAL");
        std::env::remove_var("PI_EXPERIMENTAL");
        assert!(parse_command(&["server".to_string()]).is_err());
        std::env::set_var("PI_EXPERIMENTAL", "1");
        let parsed = parse_command(&[
            "client".to_string(),
            "--connect".to_string(),
            "unix:///tmp/pi.sock".to_string(),
        ])
        .unwrap();
        assert_eq!(
            parsed,
            Some(ExperimentalCommand::Client {
                connect: Some(PathBuf::from("/tmp/pi.sock")),
                auth: None,
            })
        );
        assert!(parse_command(&[
            "server".to_string(),
            "--listen".to_string(),
            "ws://localhost".to_string(),
        ])
        .is_err());
        assert_eq!(
            parse_command(&["ordinary-message".to_string(), "server".to_string()]),
            Ok(None)
        );
        match previous {
            Some(value) => std::env::set_var("PI_EXPERIMENTAL", value),
            None => std::env::remove_var("PI_EXPERIMENTAL"),
        }
    }
}
