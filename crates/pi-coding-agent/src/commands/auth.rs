//! Auth commands — port of `packages/coding-agent/src/cli/auth-command.ts`,
//! `cli/auth-check.ts`, `cli/credential-print.ts` and the `runAuthCommand`
//! dispatch in `main.ts`.
//!
//! Surface:
//! - `pi auth print-api-key --provider <provider> [--model <model>]`
//! - `pi auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]`
//! - `pi auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]`
//! - `pi auth` / `pi auth help` prints command help.
//!
//! The upstream model runtime refreshes OAuth tokens and resolves env/
//! command API keys through the provider auth layer; this port refreshes
//! stored OAuth credentials through the auth-storage port and resolves env
//! templates through resolve_config_value.

use std::future::Future;
use std::path::Path;

use serde_json::json;

use crate::args::{parse_args, ParseOutcome};
use crate::config::{self, APP_NAME};
use crate::core::auth_storage::{
    read_stored_credential, refresh_oauth_credential_in_storage, AuthStorage, Credential,
};
use crate::core::model_registry::ModelRegistry;
use crate::core::model_resolver::{resolve_cli_model, RegistryView};

const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCommandKind {
    Check,
    ApiKey,
    BearerToken,
}

#[derive(Debug, Clone)]
pub struct AuthCommand {
    pub kind: AuthCommandKind,
    pub args: Vec<String>,
    pub json: bool,
    pub credentials: bool,
    pub no_refresh: bool,
    pub min_expiry_ms: Option<u64>,
}

pub struct AuthCommandError(pub String);

pub fn get_auth_command_name(kind: AuthCommandKind) -> &'static str {
    match kind {
        AuthCommandKind::Check => "auth check",
        AuthCommandKind::ApiKey => "auth print-api-key",
        AuthCommandKind::BearerToken => "auth print-bearer-token",
    }
}

pub fn get_auth_command_usage(kind: AuthCommandKind) -> String {
    match kind {
        AuthCommandKind::Check => {
            format!("{APP_NAME} auth check --provider <provider> [--json] [--credentials] [--no-refresh]")
        }
        AuthCommandKind::ApiKey => {
            format!("{APP_NAME} auth print-api-key --provider <provider> [--model <model>]")
        }
        AuthCommandKind::BearerToken => {
            format!("{APP_NAME} auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]")
        }
    }
}

pub fn is_auth_command_help(args: &[String]) -> bool {
    args.first().map(|a| a == "auth").unwrap_or(false)
        && (args.get(1).map(|a| a == "help").unwrap_or(true)
            || args.iter().any(|a| a == "--help" || a == "-h"))
}

pub fn print_auth_command_help() {
    println!("Usage:");
    println!("  pi auth print-api-key [--provider <provider>] [--model <model>]");
    println!("  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]");
    println!("  pi auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]");
    println!();
    println!("Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.");
}

/// Port of `parseAuthCommand(args)`.
pub fn parse_auth_command(args: &[String]) -> Result<Option<AuthCommand>, String> {
    if args.first().map(|a| a.as_str()).unwrap_or("") != "auth" {
        return Ok(None);
    }
    let kind = match args.get(1).map(|s| s.as_str()) {
        Some("check") => AuthCommandKind::Check,
        Some("print-api-key") => AuthCommandKind::ApiKey,
        Some("print-bearer-token") => AuthCommandKind::BearerToken,
        _ => {
            return Err(format!(
                "Unknown auth command \"{}\". Use \"{APP_NAME} auth print-api-key\", \"{APP_NAME} auth print-bearer-token\", or \"{APP_NAME} auth check\".",
                args.get(1).cloned().unwrap_or_default()
            ));
        }
    };

    let mut command_args: Vec<String> = Vec::new();
    let mut json = false;
    let mut credentials = false;
    let mut no_refresh = false;
    let mut min_expiry_ms: Option<u64> = None;
    let mut index = 2usize;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--min-expiry" {
            if kind != AuthCommandKind::BearerToken {
                return Err("--min-expiry is only supported by print-bearer-token".to_string());
            }
            let value = args.get(index + 1).cloned();
            let Some(value) = value else {
                return Err("--min-expiry must use a duration such as 30m or 1h".to_string());
            };
            let Some(m) = parse_min_expiry(&value) else {
                return Err("--min-expiry must use a duration such as 30m or 1h".to_string());
            };
            min_expiry_ms = Some(m);
            index += 2;
            continue;
        }
        if arg == "--json" || arg == "--credentials" || arg == "--no-refresh" {
            if kind != AuthCommandKind::Check {
                return Err(format!("{arg} is only supported by auth check"));
            }
            match arg.as_str() {
                "--json" => json = true,
                "--credentials" => credentials = true,
                _ => no_refresh = true,
            }
            index += 1;
            continue;
        }
        command_args.push(arg.clone());
        index += 1;
    }

    Ok(Some(AuthCommand {
        kind,
        args: command_args,
        json,
        credentials,
        no_refresh,
        min_expiry_ms,
    }))
}

fn parse_min_expiry(value: &str) -> Option<u64> {
    let re = regex::Regex::new(r"^(\d+)(ms|s|m|h)$").unwrap();
    let caps = re.captures(value)?;
    let amount: u64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_lowercase();
    let multiplier = match unit.as_str() {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    Some(amount * multiplier)
}

/// Port of `validateAuthCommandArgs` against a parsed flag set.
#[derive(Debug)]
pub struct ValidatedAuthArgs {
    pub provider: Option<String>,
    pub model: Option<String>,
}

pub fn validate_auth_command_args(
    args: &crate::args::Args,
    kind: AuthCommandKind,
) -> Result<ValidatedAuthArgs, String> {
    let provider = args
        .provider
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let model = args
        .model
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if !args.unknown_flags.is_empty() {
        let option = &args.unknown_flags[0];
        return Err(format!(
            "Unknown option {option} for \"{}\".",
            get_auth_command_name(kind)
        ));
    }
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".to_string());
    }
    if provider.is_none() && model.is_none() {
        let message = if kind == AuthCommandKind::Check {
            "Auth checks require --provider <provider> or --model <model>"
        } else {
            "Credential printing requires --provider <provider> or --model <model>"
        };
        return Err(message.to_string());
    }
    Ok(ValidatedAuthArgs { provider, model })
}

/// Extract the credential value from a stored credential (upstream
/// `getAuthCredential`).
pub fn auth_credential_value(credential: &Credential) -> Option<String> {
    match credential {
        Credential::ApiKey { key, .. } => key.clone(),
        Credential::OAuth { access, .. } => Some(access.clone()),
    }
}

/// Resolve the stored credential for a provider from auth.json
/// (upstream `getProviderCredential` with refresh=false for --no-refresh).
pub fn stored_credential_for(provider: &str, auth_path: &std::path::Path) -> Option<Credential> {
    read_stored_credential(provider, auth_path)
}

/// Read a provider credential and, when requested, refresh an OAuth
/// credential whose remaining validity is below the upstream minimum window.
/// `--no-refresh` deliberately returns the stored access token even when it is
/// expired; this is the CLI's explicit escape hatch for offline inspection.
pub async fn get_provider_credential(
    provider: &str,
    registry: &ModelRegistry,
    auth_path: &Path,
    refresh: bool,
    min_expiry_ms: Option<u64>,
) -> Result<Option<Credential>, String> {
    let Some(stored) = read_stored_credential(provider, auth_path) else {
        return Ok(None);
    };
    if !refresh || !matches!(stored, Credential::OAuth { .. }) {
        return Ok(Some(stored));
    }
    let Some(provider_entry) = registry.get_provider(provider) else {
        return Ok(None);
    };
    let Some(oauth) = provider_entry.auth.oauth else {
        return Ok(None);
    };
    let storage = AuthStorage::create(auth_path.to_path_buf());
    refresh_oauth_credential_in_storage(&storage, provider, oauth, min_expiry_ms, None).await
}

/// A minimal registry view over a ModelRegistry (for resolve_cli_model).
struct RegistryViewAdapter<'a> {
    registry: &'a ModelRegistry,
    all: Vec<pi_ai::model::Model>,
}

impl<'a> RegistryView for RegistryViewAdapter<'a> {
    fn models(&self) -> &[pi_ai::model::Model] {
        &self.all
    }
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.registry.has_configured_auth(provider)
    }
}

/// Resolve a provider from a `--provider`/`--model` pair, mirroring the
/// upstream auth command's model-resolution path. Returns the provider id.
pub fn resolve_auth_provider(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    registry: &ModelRegistry,
) -> Result<String, String> {
    if let Some(model) = cli_model {
        let all = registry.get_all();
        let view = RegistryViewAdapter {
            registry,
            all: all.clone(),
        };
        let resolved = resolve_cli_model(cli_provider, Some(model), None, &view);
        if let (None, Some(error)) = (&resolved.model, &resolved.error) {
            return Err(error.clone());
        }
        if let Some(model) = resolved.model {
            return Ok(model.provider);
        }
    }
    cli_provider
        .map(|s| s.to_string())
        .ok_or_else(|| "Unable to resolve an auth provider".to_string())
}

/// Check provider auth (upstream `checkProviderAuth`, refresh simplified).
pub fn check_provider_auth(
    provider: &str,
    registry: &ModelRegistry,
    auth_path: &std::path::Path,
) -> AuthCheckResult {
    if registry.get_error().is_some() {
        return AuthCheckResult {
            status: "invalid".to_string(),
            provider: provider.to_string(),
            reason: Some("invalid_state".to_string()),
            auth_type: None,
        };
    }
    if registry.get_provider(provider).is_none() {
        return AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("provider_not_found".to_string()),
            auth_type: None,
        };
    }
    let Some(credential) = read_stored_credential(provider, auth_path) else {
        return AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("credentials_not_configured".to_string()),
            auth_type: None,
        };
    };
    AuthCheckResult {
        status: "ready".to_string(),
        provider: provider.to_string(),
        reason: None,
        auth_type: Some(
            if credential.credential_type() == "oauth" {
                "oauth"
            } else {
                "api_key"
            }
            .to_string(),
        ),
    }
}

/// Async auth check with upstream's refresh option. The status remains
/// `ready` when `refresh` is false, including for an expired OAuth token; a
/// refresh failure is reported as `invalid_state` and the old credential is
/// left in storage by the locked refresh helper.
pub async fn check_provider_auth_with_options(
    provider: &str,
    registry: &ModelRegistry,
    auth_path: &Path,
    refresh: bool,
) -> AuthCheckResult {
    if registry.get_error().is_some() {
        return AuthCheckResult {
            status: "invalid".to_string(),
            provider: provider.to_string(),
            reason: Some("invalid_state".to_string()),
            auth_type: None,
        };
    }
    let Some(provider_entry) = registry.get_provider(provider) else {
        return AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("provider_not_found".to_string()),
            auth_type: None,
        };
    };
    let Some(credential) = read_stored_credential(provider, auth_path) else {
        return AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("credentials_not_configured".to_string()),
            auth_type: None,
        };
    };
    let auth_type = if credential.credential_type() == "oauth" {
        "oauth"
    } else {
        "api_key"
    };
    if refresh && matches!(credential, Credential::OAuth { .. }) {
        if provider_entry.auth.oauth.is_none() {
            return AuthCheckResult {
                status: "not_ready".to_string(),
                provider: provider.to_string(),
                reason: Some("credentials_not_configured".to_string()),
                auth_type: None,
            };
        }
        match get_provider_credential(provider, registry, auth_path, true, None).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return AuthCheckResult {
                    status: "not_ready".to_string(),
                    provider: provider.to_string(),
                    reason: Some("credentials_not_configured".to_string()),
                    auth_type: None,
                };
            }
            Err(_) => {
                return AuthCheckResult {
                    status: "invalid".to_string(),
                    provider: provider.to_string(),
                    reason: Some("invalid_state".to_string()),
                    auth_type: None,
                };
            }
        }
    }
    AuthCheckResult {
        status: "ready".to_string(),
        provider: provider.to_string(),
        reason: None,
        auth_type: Some(auth_type.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthCheckResult {
    pub status: String,
    pub provider: String,
    pub reason: Option<String>,
    pub auth_type: Option<String>,
}

/// Build the model registry facade used by auth commands (upstream
/// `createAuthCheckModelRuntime`: no network, no refresh on create).
pub fn create_auth_check_model_registry() -> ModelRegistry {
    let models = crate::core::model_registry::builtin_models();
    let models_path = config::get_agent_dir().join("models.json");
    let config = crate::core::model_config::ModelConfig::load(Some(&models_path));
    crate::core::model_registry::ModelRegistry::new(models, config)
}

fn block_on_auth<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("auth command runtime should build")
        .block_on(future)
}

/// `pi auth` dispatcher. Returns true when the args were an auth command.
pub fn handle_auth_command(args: &[String]) -> bool {
    if is_auth_command_help(args) {
        print_auth_command_help();
        return true;
    }
    let command = match parse_auth_command(args) {
        Ok(Some(command)) => command,
        Ok(None) => return false,
        Err(message) => {
            eprintln!("Error: {message}");
            auth_exit(if args.len() > 1 && args[1] == "print-api-key" {
                1
            } else {
                2
            });
        }
    };

    // Re-parse the remaining command args with the normal flag parser so
    // --provider/--model/--api-key etc. resolve identically.
    let parsed = match parse_args(&command.args) {
        ParseOutcome::Run(args) => args,
        ParseOutcome::Help | ParseOutcome::Version => {
            // Unreachable for auth sub-args.
            return true;
        }
    };
    if !parsed.unknown_flags.is_empty() {
        let option = &parsed.unknown_flags[0];
        eprintln!(
            "Unknown option {option} for \"{}\".",
            get_auth_command_name(command.kind)
        );
        eprintln!(
            "Use \"{APP_NAME} --help\" or \"{}\".",
            get_auth_command_usage(command.kind)
        );
        std::process::exit(1);
    }
    let validated = match validate_auth_command_args(&parsed, command.kind) {
        Ok(validated) => validated,
        Err(message) => {
            eprintln!("Error: {message}");
            std::process::exit(if command.kind == AuthCommandKind::Check {
                2
            } else {
                1
            });
        }
    };

    let registry = create_auth_check_model_registry();
    let auth_path = config::get_auth_path();

    if command.kind != AuthCommandKind::Check {
        // print-api-key / print-bearer-token.
        let provider = match resolve_auth_provider(
            validated.provider.as_deref(),
            validated.model.as_deref(),
            &registry,
        ) {
            Ok(provider) => provider,
            Err(message) => {
                eprintln!("Error: {message}");
                std::process::exit(1);
            }
        };
        if registry.get_provider(&provider).is_none() {
            eprintln!("Error: Unknown provider \"{provider}\". Use --list-models to see available providers.");
            std::process::exit(1);
        }
        let refresh = command.kind == AuthCommandKind::BearerToken;
        let credential = match block_on_auth(get_provider_credential(
            &provider,
            &registry,
            &auth_path,
            refresh,
            if refresh {
                command
                    .min_expiry_ms
                    .or(Some(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS))
            } else {
                None
            },
        )) {
            Ok(credential) => credential,
            Err(error) => {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
        };
        let Some(credential) = credential else {
            let (verb, noun) = match command.kind {
                AuthCommandKind::ApiKey => ("API key", "API key"),
                _ => ("OAuth bearer token", "OAuth bearer token"),
            };
            let _ = verb;
            eprintln!("Error: No usable {noun} is configured");
            std::process::exit(1);
        };
        let credential_type = credential.credential_type();
        if command.kind == AuthCommandKind::ApiKey && credential_type == "oauth" {
            eprintln!("Error: Provider \"{provider}\" is configured with OAuth, not an API key");
            std::process::exit(1);
        }
        if command.kind == AuthCommandKind::BearerToken && credential_type != "oauth" {
            eprintln!(
                "Error: Provider \"{provider}\" is not configured with an OAuth bearer token"
            );
            std::process::exit(1);
        }
        let Some(value) = auth_credential_value(&credential) else {
            eprintln!(
                "Error: No usable {} is configured",
                if command.kind == AuthCommandKind::ApiKey {
                    "API key"
                } else {
                    "OAuth bearer token"
                }
            );
            std::process::exit(1);
        };
        println!("{value}");
        return true;
    }

    // auth check.
    {
        let mut result = block_on_auth(check_provider_auth_with_options(
            validated.provider.as_deref().unwrap_or(""),
            &registry,
            &auth_path,
            !command.no_refresh,
        ));
        let mut credential_value: Option<String> = None;
        if command.credentials && result.status == "ready" {
            match block_on_auth(get_provider_credential(
                &result.provider,
                &registry,
                &auth_path,
                !command.no_refresh,
                None,
            )) {
                Ok(Some(credential)) => {
                    credential_value = auth_credential_value(&credential);
                }
                Ok(None) | Err(_) => {
                    result = AuthCheckResult {
                        status: "not_ready".to_string(),
                        provider: result.provider.clone(),
                        reason: Some("credential_not_available".to_string()),
                        auth_type: None,
                    };
                }
            }
        }
        let output = if command.json {
            let mut obj = json!({
                "status": result.status,
                "provider": result.provider,
            });
            if let Some(reason) = &result.reason {
                obj["reason"] = json!(reason);
            }
            if let Some(auth_type) = &result.auth_type {
                obj["authType"] = json!(auth_type);
            }
            if let Some(value) = &credential_value {
                obj["credentials"] = json!(value);
            }
            serde_json::to_string(&obj).unwrap()
        } else if let Some(value) = credential_value {
            value
        } else {
            result.status.clone()
        };
        println!("{output}");
        match result.status.as_str() {
            "ready" => std::process::exit(0),
            "not_ready" => std::process::exit(1),
            _ => std::process::exit(2),
        }
    }
}

/// Internal exit for auth-check: parse errors exit 2; other auth errors exit 1.
fn auth_exit(code: i32) -> ! {
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_check_command() {
        let args = vec![
            "auth".to_string(),
            "check".to_string(),
            "--provider".to_string(),
            "google".to_string(),
        ];
        let command = parse_auth_command(&args).unwrap().unwrap();
        assert_eq!(command.kind, AuthCommandKind::Check);
        assert_eq!(command.args, vec!["--provider", "google"]);
    }

    #[test]
    fn parses_check_json_credentials() {
        let args = vec![
            "auth".to_string(),
            "check".to_string(),
            "--json".to_string(),
            "--credentials".to_string(),
            "--no-refresh".to_string(),
            "--provider".to_string(),
            "x".to_string(),
        ];
        let command = parse_auth_command(&args).unwrap().unwrap();
        assert!(command.json);
        assert!(command.credentials);
        assert!(command.no_refresh);
    }

    #[test]
    fn parses_print_api_key() {
        let args = vec![
            "auth".to_string(),
            "print-api-key".to_string(),
            "--provider".to_string(),
            "google".to_string(),
        ];
        let command = parse_auth_command(&args).unwrap().unwrap();
        assert_eq!(command.kind, AuthCommandKind::ApiKey);
        assert_eq!(command.args, vec!["--provider", "google"]);
    }

    #[test]
    fn min_expiry_parsed_units() {
        assert_eq!(parse_min_expiry("30m"), Some(30 * 60_000));
        assert_eq!(parse_min_expiry("1h"), Some(3_600_000));
        assert_eq!(parse_min_expiry("5s"), Some(5_000));
        assert_eq!(parse_min_expiry("250ms"), Some(250));
        assert_eq!(parse_min_expiry("10x"), None);
    }

    #[test]
    fn min_expiry_only_for_bearer() {
        let args = vec![
            "auth".to_string(),
            "print-api-key".to_string(),
            "--min-expiry".to_string(),
            "30m".to_string(),
        ];
        let err = parse_auth_command(&args).unwrap_err();
        assert!(err.contains("--min-expiry"));
    }

    #[test]
    fn json_flag_only_for_check() {
        let args = vec![
            "auth".to_string(),
            "print-api-key".to_string(),
            "--json".to_string(),
        ];
        let err = parse_auth_command(&args).unwrap_err();
        assert!(err.contains("only supported by auth check"));
    }

    #[test]
    fn unknown_auth_command_errors() {
        let args = vec!["auth".to_string(), "frobnicate".to_string()];
        let err = parse_auth_command(&args).unwrap_err();
        assert!(err.contains("Unknown auth command"));
    }

    #[test]
    fn not_auth_command() {
        let args = vec!["auth.z".to_string()];
        assert!(parse_auth_command(&args).unwrap().is_none());
    }

    #[test]
    fn is_help_detection() {
        assert!(is_auth_command_help(&["auth".to_string()]));
        assert!(is_auth_command_help(&[
            "auth".to_string(),
            "help".to_string()
        ]));
        assert!(is_auth_command_help(&[
            "auth".to_string(),
            "--help".to_string()
        ]));
        assert!(!is_auth_command_help(&[
            "auth".to_string(),
            "check".to_string()
        ]));
    }

    #[test]
    fn validate_rejects_non_provider_model_flags() {
        let args = parse_args(&["--api-key".to_string(), "k".to_string()]).expect_run();
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.contains("Auth commands only accept --provider and --model"));
    }

    #[test]
    fn validate_requires_provider_or_model() {
        let args = parse_args(&[]).expect_run();
        let err = validate_auth_command_args(&args, AuthCommandKind::Check).unwrap_err();
        assert!(err.contains("require --provider <provider> or --model <model>"));
    }

    #[test]
    fn credential_value_extraction() {
        let api = crate::core::auth_storage::Credential::ApiKey {
            key: Some("k".into()),
            env: None,
        };
        assert_eq!(auth_credential_value(&api).as_deref(), Some("k"));
        let oauth = crate::core::auth_storage::Credential::OAuth {
            access: "access-token".into(),
            refresh: "r".into(),
            expires: 1,
            extra: Default::default(),
        };
        assert_eq!(
            auth_credential_value(&oauth).as_deref(),
            Some("access-token")
        );
    }

    #[test]
    fn auth_help_text_lists_commands() {
        // Smoke: the usage text mentions all three commands.
        assert!(get_auth_command_name(AuthCommandKind::Check).contains("auth check"));
        assert!(get_auth_command_usage(AuthCommandKind::ApiKey).contains("print-api-key"));
        assert!(get_auth_command_usage(AuthCommandKind::BearerToken).contains("print-bearer-token"));
    }
}
