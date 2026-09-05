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

fn is_truthy_env_value(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

pub(crate) fn nonempty_env_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn home_dir_from_values(
    home: Option<&str>,
    userprofile: Option<&str>,
    fallback: Option<PathBuf>,
) -> Option<PathBuf> {
    // Node's `os.homedir()` follows the host platform: Unix uses HOME (then
    // the passwd database), while Windows uses USERPROFILE. Do not let a
    // Windows-only variable redirect a Unix agent's config/session roots.
    #[cfg(windows)]
    let preferred = userprofile
        .filter(|value| !value.is_empty())
        .or_else(|| home.filter(|value| !value.is_empty()));
    #[cfg(not(windows))]
    let preferred = {
        let _ = userprofile;
        home.filter(|value| !value.is_empty())
    };

    preferred.map(PathBuf::from).or(fallback)
}

/// Resolve the user's home using the environment precedence used by the
/// upstream runtime, with the platform resolver as a final fallback.
pub fn home_dir() -> Option<PathBuf> {
    let home = env("HOME");
    let userprofile = env("USERPROFILE");
    home_dir_from_values(home.as_deref(), userprofile.as_deref(), dirs::home_dir())
}

pub fn env_flag(name: &str) -> bool {
    env(name).is_some_and(|value| is_truthy_env_value(&value))
}

/// Expands a leading `~` to the home directory (returns the input unchanged
/// when no home is available).
pub fn expand_tilde_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    } else if path == "~" {
        if let Some(home) = home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// `~/.pi/agent` (or `PI_CODING_AGENT_DIR`).
pub fn get_agent_dir() -> PathBuf {
    if let Some(dir) = nonempty_env_value(env(ENV_AGENT_DIR)) {
        return PathBuf::from(expand_tilde_path(&dir));
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_DIR_NAME)
        .join("agent")
}

/// `getAgentDir()/sessions` (or `PI_CODING_AGENT_SESSION_DIR`).
pub fn get_session_dir() -> PathBuf {
    if let Some(dir) = nonempty_env_value(env(ENV_SESSION_DIR)) {
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
        .filter(|provider| !provider.is_empty())
        .map(|s| s.to_string())
        .or_else(|| nonempty_env_value(env(ENV_PROVIDER)))
        .unwrap_or_else(|| "google".to_string())
}

pub fn resolve_model(cli_model: Option<&str>) -> Option<String> {
    cli_model
        .filter(|model| !model.is_empty())
        .map(|s| s.to_string())
        .or_else(|| nonempty_env_value(env(ENV_MODEL)))
}

/// Raw `PI_REASONING_LEVEL` environment value (empty counts as unset).
/// Validation against the upstream level set happens at the thinking
/// resolution sites so CLI, RPC, and interactive callers share one rule.
pub(crate) fn env_reasoning_level() -> Option<String> {
    nonempty_env_value(env(ENV_REASONING_LEVEL))
}

pub fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::{home_dir_from_values, is_truthy_env_value, nonempty_env_value};

    #[test]
    fn env_flag_truthiness_matches_upstream() {
        for value in ["1", "true", "TRUE", "yes", "YeS"] {
            assert!(is_truthy_env_value(value), "expected {value:?} to be true");
        }
        for value in ["", "0", "false", "on", "ON", "random"] {
            assert!(
                !is_truthy_env_value(value),
                "expected {value:?} to be false"
            );
        }
    }

    #[test]
    fn empty_path_environment_values_fall_back_to_defaults() {
        assert_eq!(nonempty_env_value(None), None);
        assert_eq!(nonempty_env_value(Some(String::new())), None);
        assert_eq!(
            nonempty_env_value(Some("/tmp/pi-agent".to_owned())),
            Some("/tmp/pi-agent".to_owned())
        );
    }

    #[test]
    fn home_environment_uses_host_platform_precedence_then_platform_fallback() {
        #[cfg(windows)]
        assert_eq!(
            home_dir_from_values(Some("C:\\home"), Some("C:\\Users\\pi"), None),
            Some(PathBuf::from("C:\\Users\\pi"))
        );
        #[cfg(not(windows))]
        assert_eq!(
            home_dir_from_values(Some("/home/pi"), Some("C:\\Users\\pi"), None),
            Some(PathBuf::from("/home/pi"))
        );
        #[cfg(windows)]
        assert_eq!(
            home_dir_from_values(Some(""), Some("C:\\Users\\pi"), None),
            Some(PathBuf::from("C:\\Users\\pi"))
        );
        #[cfg(not(windows))]
        assert_eq!(
            home_dir_from_values(
                Some(""),
                Some("C:\\Users\\pi"),
                Some(PathBuf::from("/fallback"))
            ),
            Some(PathBuf::from("/fallback"))
        );
    }

    // Local serializing lock for process-environment mutation. This test
    // module is also compiled into the `llama_parity` integration target via
    // `#[path]`, so it must not reference `crate::core` or tokio here; the
    // sanctioned suites run with `--test-threads=1`.
    static ENV_PRECEDENCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_PRECEDENCE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn resolve_provider_prefers_cli_over_env_over_default() {
        let _guard = lock_env();
        let _provider = EnvGuard::remove(super::ENV_PROVIDER);

        assert_eq!(super::resolve_provider(None), "google");
        assert_eq!(super::resolve_provider(Some("")), "google");

        let _env = EnvGuard::set(super::ENV_PROVIDER, "faux");
        assert_eq!(super::resolve_provider(None), "faux");
        assert_eq!(super::resolve_provider(Some("other")), "other");
        // An empty CLI value does not mask the environment.
        assert_eq!(super::resolve_provider(Some("")), "faux");

        let _empty = EnvGuard::set(super::ENV_PROVIDER, "");
        assert_eq!(super::resolve_provider(None), "google");
    }

    #[test]
    fn resolve_model_prefers_cli_over_env() {
        let _guard = lock_env();
        let _model = EnvGuard::remove(super::ENV_MODEL);

        assert_eq!(super::resolve_model(None), None);
        assert_eq!(super::resolve_model(Some("")), None);

        let _env = EnvGuard::set(super::ENV_MODEL, "faux-1");
        assert_eq!(super::resolve_model(None), Some("faux-1".to_string()));
        assert_eq!(
            super::resolve_model(Some("other-1")),
            Some("other-1".to_string())
        );
        // An empty CLI value does not mask the environment.
        assert_eq!(super::resolve_model(Some("")), Some("faux-1".to_string()));

        let _empty = EnvGuard::set(super::ENV_MODEL, "");
        assert_eq!(super::resolve_model(None), None);
    }
}
