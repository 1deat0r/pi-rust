//! Server option and service types — port of `packages/server/src/types.ts`.

use pi_protocol::{ModelRef, ThinkingLevel};

use crate::listener::PiServerListener;

pub struct PiServerOptions {
    pub listeners: Vec<Box<dyn PiServerListener>>,
    pub max_frame_length: Option<u64>,
    pub handshake_timeout_ms: Option<u64>,
    pub server_id: Option<String>,
    pub on_error: Option<ArcErrorObserver>,
}

pub type ArcErrorObserver = std::sync::Arc<dyn Fn(std::io::Error) + Send + Sync>;

/// Prompt input (Command::Prompt minus command/sessionId).
#[derive(Debug, Clone)]
pub struct PromptInput {
    pub text: String,
}

/// Steer input (Command::Steer minus command/sessionId).
#[derive(Debug, Clone)]
pub struct SteerInput {
    pub text: String,
}

pub use crate::service::PiServerService;

pub struct CreateSessionOptions {
    pub id: String,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub model: Option<ModelRef>,
    pub thinking_level: Option<ThinkingLevel>,
}

pub use crate::service::PiSessionRuntime;
