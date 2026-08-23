//! App/path/env configuration — port of `packages/coding-agent/src/config.ts`
//! (paths and environment variable names).

use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "pi";
pub const APP_TITLE: &str = "π";
pub const CONFIG_DIR_NAME: &str = ".pi";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";
pub const ENV_MODEL: &str = "PI_MODEL";
pub const ENV_PROVIDER: &str = "PI_PROVIDER";
pub const ENV_KEY: &str = "PI_KEY";
pub const ENV_SESSION_ID: &str = "PI_SESSION_ID";
pub const ENV_SESSION_FILE: &str = "PI_SESSION_FILE";
pub const ENV_OFFLINE: &str = "PI_OFFLINE";
pub const ENV_REASONING_LEVEL: &str = "PI_REASONING_LEVEL";
pub const ENV_TELEMETRY: &str = "PI_TELEMETRY";
pub const ENV_SKIP_VERSION_CHECK: &str = "PI_SKIP_VERSION_CHECK";

pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Expands a leading `~` to the home directory (returns the input unchanged
/// when no home is available).
pub fn expand_tilde_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// `~/.pi/agent` (or `PI_CODING_AGENT_DIR`).
pub fn get_agent_dir() -> PathBuf {
    if let Some(dir) = env(ENV_AGENT_DIR) {
        return PathBuf::from(expand_tilde_path(&dir));
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR_NAME)
        .join("agent")
}

/// `getAgentDir()/sessions` (or `PI_CODING_AGENT_SESSION_DIR`).
pub fn get_session_dir() -> PathBuf {
    if let Some(dir) = env(ENV_SESSION_DIR) {
        return PathBuf::from(expand_tilde_path(&dir));
    }
    get_agent_dir().join("sessions")
}

pub fn get_settings_path() -> PathBuf {
    get_agent_dir().join("settings.json")
}

pub fn get_auth_path() -> PathBuf {
    get_agent_dir().join("auth.json")
}

/// Resolves a provider from the explicit argument, then `PI_PROVIDER`, then
/// the upstream default `google`.
pub fn resolve_provider(cli_provider: Option<&str>) -> String {
    cli_provider
        .map(|s| s.to_string())
        .or_else(|| env(ENV_PROVIDER))
        .unwrap_or_else(|| "google".to_string())
}

pub fn resolve_model(cli_model: Option<&str>) -> Option<String> {
    cli_model.map(|s| s.to_string()).or_else(|| env(ENV_MODEL))
}

pub fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}
