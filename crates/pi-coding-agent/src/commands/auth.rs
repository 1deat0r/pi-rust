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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::args::{parse_args, ParseOutcome};
use crate::config::{self, APP_NAME};
use crate::core::auth_storage::{
    read_stored_credential, refresh_oauth_credential_in_storage, AuthStorage, Credential,
    ReadOnlyAuthStorage,
};
use crate::core::model_config::{ModelConfig, ModelsJsonProvider};
use crate::core::model_registry::ModelRegistry;
use crate::core::model_resolver::{resolve_cli_model, RegistryView};
use crate::core::provider_composer::{
    apply_model_overrides, apply_models_json, config_value_env_var_names, is_command_config_value,
};
use crate::core::resolve_config_value::resolve_config_value;

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

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
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

/// Extract a printable credential from the resolved request auth returned by
/// pi-ai. This mirrors upstream `getAuthCredential`: ordinary providers use
/// `auth.apiKey`, while providers such as Anthropic may expose a bearer token
/// only through an `Authorization` header.
pub fn auth_result_credential_value(auth: &pi_ai::auth::AuthResult) -> Option<String> {
    if let Some(api_key) = auth.auth.api_key.as_deref() {
        if !api_key.trim().is_empty() {
            return Some(api_key.to_string());
        }
    }
    auth.auth.headers.as_ref().and_then(|headers| {
        headers.iter().find_map(|(name, value)| {
            if !name.eq_ignore_ascii_case("authorization") {
                return None;
            }
            let value = value.as_deref()?.trim();
            let (scheme, token) = value.split_once(char::is_whitespace)?;
            (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
                .then(|| token.trim().to_string())
        })
    })
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
    let storage = AuthStorage::create(auth_path.to_path_buf());
    let stored = storage
        .read(
            provider,
            &crate::core::auth_storage::AuthOperationOptions::default(),
        )
        .await
        .map_err(|error| format!("Failed to read auth credentials: {error}"))?;
    if let Some(stored) = stored {
        if !refresh || !matches!(stored, Credential::OAuth { .. }) {
            return Ok(Some(stored));
        }
        let Some(provider_entry) = registry.get_provider(provider) else {
            return Ok(Some(stored));
        };
        let Some(oauth) = provider_entry.auth.oauth else {
            // Preserve a stored OAuth bearer token when this provider has no
            // refresh implementation. The upstream credential-print path
            // still recognizes the stored credential type and returns its
            // access token.
            return Ok(Some(stored));
        };
        return refresh_oauth_credential_in_storage(&storage, provider, oauth, min_expiry_ms, None)
            .await
            .map_err(|e| e.to_string());
    }

    let config = load_auth_model_config();
    let configured_key = config
        .get_provider(provider)
        .and_then(|provider| provider.api_key.as_deref());
    if configured_key.is_some() {
        if let Some(value) = configured_api_key_value(config.get_provider(provider)) {
            return Ok(Some(Credential::ApiKey {
                key: Some(value),
                env: None,
            }));
        }
        // An explicitly configured but unresolved key takes precedence over
        // inherited ambient auth, just as composeApiKeyAuth does upstream.
        return Ok(None);
    }

    // No file credential or overriding models.json key: resolve ambient
    // provider auth (environment, provider-specific files, or other pi-ai
    // auth sources) exactly as a normal request would. The value is
    // materialized only for explicit credential-printing or
    // `auth check --credentials` paths.
    if let Some(auth) = registry.models_facade().get_auth(provider, None) {
        if let Some(value) = auth_result_credential_value(&auth) {
            return Ok(Some(Credential::ApiKey {
                key: Some(value),
                env: auth.env,
            }));
        }
    }

    Ok(None)
}

fn auth_model_config_path() -> PathBuf {
    config::get_agent_dir().join("models.json")
}

fn load_auth_model_config() -> ModelConfig {
    ModelConfig::load(Some(&auth_model_config_path()))
}

/// Add models.json-only providers to the catalog used by auth model
/// resolution. ModelRegistry already overlays models.json onto bundled
/// providers; this fills the remaining upstream case where a provider exists
/// only in models.json.
fn auth_model_catalog(registry: &ModelRegistry, config: &ModelConfig) -> Vec<pi_ai::model::Model> {
    let mut models = registry.get_all();
    for provider_id in config.get_provider_ids() {
        if registry.get_provider(provider_id).is_some() {
            continue;
        }
        let Some(provider_config) = config.get_provider(provider_id) else {
            continue;
        };
        let Ok(custom_models) = apply_models_json(provider_id, &[], Some(provider_config)) else {
            continue;
        };
        models.extend(apply_model_overrides(custom_models, Some(provider_config)));
    }
    models.sort_by(|left, right| {
        format!("{}/{}", left.provider, left.id).cmp(&format!("{}/{}", right.provider, right.id))
    });
    models.dedup_by(|left, right| left.provider == right.provider && left.id == right.id);
    models
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAuthSource {
    auth_type: String,
    source: Option<String>,
}

fn configured_api_key_status(config: Option<&ModelsJsonProvider>) -> Option<ResolvedAuthSource> {
    let raw_key = config?.api_key.as_deref()?;
    if is_command_config_value(raw_key) {
        // Pi treats a configured command as an available credential during a
        // check; execution is deferred until a request actually resolves it.
        return Some(ResolvedAuthSource {
            auth_type: "api_key".to_string(),
            source: Some("configured API key".to_string()),
        });
    }
    let env_names = config_value_env_var_names(raw_key);
    if env_names.iter().any(|name| {
        std::env::var(name)
            .ok()
            .is_none_or(|value| value.trim().is_empty())
    }) {
        return None;
    }
    let value = resolve_config_value(raw_key, None)?;
    if value.trim().is_empty() {
        return None;
    }
    Some(ResolvedAuthSource {
        auth_type: "api_key".to_string(),
        source: Some("configured API key".to_string()),
    })
}

fn configured_api_key_value(config: Option<&ModelsJsonProvider>) -> Option<String> {
    let raw_key = config?.api_key.as_deref()?;
    let value = resolve_config_value(raw_key, None)?;
    (!value.trim().is_empty()).then_some(value)
}

fn provider_configured_from_facade(
    provider: &str,
    registry: &ModelRegistry,
    config: &ModelConfig,
) -> bool {
    if config
        .get_provider(provider)
        .and_then(|provider| provider.api_key.as_deref())
        .is_some()
    {
        return configured_api_key_status(config.get_provider(provider)).is_some();
    }
    registry.models_facade().check_auth(provider).is_some()
}

/// A minimal registry view over a ModelRegistry (for resolve_cli_model).
struct RegistryViewAdapter<'a> {
    all: Vec<pi_ai::model::Model>,
    configured_providers: BTreeSet<String>,
    _registry: std::marker::PhantomData<&'a ModelRegistry>,
}

impl<'a> RegistryView for RegistryViewAdapter<'a> {
    fn models(&self) -> &[pi_ai::model::Model] {
        &self.all
    }
    fn has_configured_auth(&self, provider: &str) -> bool {
        self.configured_providers.contains(provider)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAuthTarget {
    pub provider: String,
    pub model: Option<pi_ai::model::Model>,
}

/// Resolve the effective provider and, when supplied, the effective model.
/// This is the auth-command equivalent of upstream `resolveCliModel` and
/// includes models.json-only model definitions.
pub fn resolve_auth_target(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    registry: &ModelRegistry,
) -> Result<ResolvedAuthTarget, String> {
    resolve_auth_target_with_config(cli_provider, cli_model, registry, &load_auth_model_config())
}

fn resolve_auth_target_with_config(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    registry: &ModelRegistry,
    config: &ModelConfig,
) -> Result<ResolvedAuthTarget, String> {
    if let Some(model_hint) = cli_model {
        let all = auth_model_catalog(registry, config);
        let configured_providers = registry
            .models_facade()
            .get_providers()
            .into_iter()
            .map(|provider| provider.id)
            .chain(config.get_provider_ids().map(str::to_string))
            .filter(|provider| provider_configured_from_facade(provider, registry, config))
            .collect();
        let view = RegistryViewAdapter {
            all,
            configured_providers,
            _registry: std::marker::PhantomData,
        };
        let resolved = resolve_cli_model(cli_provider, Some(model_hint), None, &view);
        if let Some(error) = resolved.error {
            if resolved.model.is_none() {
                return Err(error);
            }
        }
        if let Some(model) = resolved.model {
            return Ok(ResolvedAuthTarget {
                provider: model.provider.clone(),
                model: Some(model),
            });
        }
    }
    cli_provider
        .map(|provider| ResolvedAuthTarget {
            provider: provider.to_string(),
            model: None,
        })
        .ok_or_else(|| "Unable to resolve an auth provider".to_string())
}

/// Resolve a provider from a `--provider`/`--model` pair, mirroring the
/// upstream auth command's model-resolution path. Returns the provider id.
pub fn resolve_auth_provider(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    registry: &ModelRegistry,
) -> Result<String, String> {
    resolve_auth_target(cli_provider, cli_model, registry).map(|target| target.provider)
}

fn auth_result_from_source(provider: &str, source: Option<ResolvedAuthSource>) -> AuthCheckResult {
    match source {
        Some(source) => AuthCheckResult {
            status: "ready".to_string(),
            provider: provider.to_string(),
            reason: None,
            auth_type: Some(source.auth_type),
        },
        None => AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("credentials_not_configured".to_string()),
            auth_type: None,
        },
    }
}

fn provider_exists(provider: &str, registry: &ModelRegistry, config: &ModelConfig) -> bool {
    registry.get_provider(provider).is_some() || config.get_provider(provider).is_some()
}

fn source_from_facade(provider: &str, registry: &ModelRegistry) -> Option<ResolvedAuthSource> {
    registry
        .models_facade()
        .check_auth(provider)
        .map(|check| ResolvedAuthSource {
            auth_type: check.auth_type.to_string(),
            source: check.source,
        })
}

fn source_from_stored(
    provider_entry: Option<&pi_ai::models::Provider>,
    credential: &Credential,
    config: Option<&ModelsJsonProvider>,
) -> Option<ResolvedAuthSource> {
    match credential {
        Credential::OAuth { .. } => provider_entry
            .and_then(|provider| provider.auth.oauth.as_ref())
            .map(|_| ResolvedAuthSource {
                auth_type: "oauth".to_string(),
                source: Some("OAuth".to_string()),
            }),
        Credential::ApiKey { key, env } => {
            if let Some(auth) = provider_entry.and_then(|provider| provider.auth.api_key.as_ref()) {
                let credential = pi_ai::auth::ApiKeyCredential {
                    key: key.clone(),
                    env: env.clone(),
                };
                return auth
                    .check(&pi_ai::auth::AuthContext::default(), Some(&credential))
                    .map(|check| ResolvedAuthSource {
                        auth_type: check.auth_type.to_string(),
                        source: check.source,
                    });
            }

            // models.json-only providers are composed into the upstream
            // runtime even though this Rust registry has no low-level
            // provider object for them. A stored non-empty key is therefore
            // usable when the config declares an API-key auth method.
            (config?.api_key.is_some() && key.as_deref().is_some_and(|key| !key.trim().is_empty()))
                .then(|| ResolvedAuthSource {
                    auth_type: "api_key".to_string(),
                    source: Some("stored credential".to_string()),
                })
        }
    }
}

fn auth_check_sync_with_config(
    provider: &str,
    registry: &ModelRegistry,
    auth_path: &Path,
    config: &ModelConfig,
) -> AuthCheckResult {
    if registry.get_error().is_some() {
        return AuthCheckResult {
            status: "invalid".to_string(),
            provider: provider.to_string(),
            reason: Some("invalid_state".to_string()),
            auth_type: None,
        };
    }
    if !provider_exists(provider, registry, config) {
        return AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("provider_not_found".to_string()),
            auth_type: None,
        };
    }
    if let Some(stored) = read_stored_credential(provider, auth_path) {
        return auth_result_from_source(
            provider,
            source_from_stored(
                registry.get_provider(provider).as_ref(),
                &stored,
                config.get_provider(provider),
            ),
        );
    }
    if config
        .get_provider(provider)
        .and_then(|provider| provider.api_key.as_deref())
        .is_some()
    {
        return auth_result_from_source(
            provider,
            configured_api_key_status(config.get_provider(provider)),
        );
    }
    if let Some(source) = source_from_facade(provider, registry) {
        return auth_result_from_source(provider, Some(source));
    }
    auth_result_from_source(
        provider,
        configured_api_key_status(config.get_provider(provider)),
    )
}

/// Check provider auth (upstream `checkProviderAuth`, synchronous facade).
pub fn check_provider_auth(
    provider: &str,
    registry: &ModelRegistry,
    auth_path: &std::path::Path,
) -> AuthCheckResult {
    auth_check_sync_with_config(provider, registry, auth_path, &load_auth_model_config())
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
    check_provider_auth_with_options_and_config(
        provider,
        registry,
        auth_path,
        refresh,
        &load_auth_model_config(),
    )
    .await
}

async fn check_provider_auth_with_options_and_config(
    provider: &str,
    registry: &ModelRegistry,
    auth_path: &Path,
    refresh: bool,
    config: &ModelConfig,
) -> AuthCheckResult {
    if registry.get_error().is_some() {
        return AuthCheckResult {
            status: "invalid".to_string(),
            provider: provider.to_string(),
            reason: Some("invalid_state".to_string()),
            auth_type: None,
        };
    }
    if !provider_exists(provider, registry, config) {
        return AuthCheckResult {
            status: "not_ready".to_string(),
            provider: provider.to_string(),
            reason: Some("provider_not_found".to_string()),
            auth_type: None,
        };
    }

    // `--no-refresh` uses upstream ReadOnlyAuthStorage, whose strict load
    // validation makes malformed auth.json an invalid state. The normal
    // refresh path uses AuthStorage and preserves its fail-open read snapshot.
    let options = crate::core::auth_storage::AuthOperationOptions::default();
    let stored = if refresh {
        AuthStorage::create(auth_path.to_path_buf())
            .read(provider, &options)
            .await
    } else {
        ReadOnlyAuthStorage::new(auth_path.to_path_buf())
            .read(provider, &options)
            .await
    };
    let stored = match stored {
        Ok(stored) => stored,
        Err(_) => {
            return AuthCheckResult {
                status: "invalid".to_string(),
                provider: provider.to_string(),
                reason: Some("invalid_state".to_string()),
                auth_type: None,
            }
        }
    };

    if let Some(credential) = stored {
        if refresh && matches!(credential, Credential::OAuth { .. }) {
            let Some(provider_entry) = registry.get_provider(provider) else {
                return AuthCheckResult {
                    status: "not_ready".to_string(),
                    provider: provider.to_string(),
                    reason: Some("credentials_not_configured".to_string()),
                    auth_type: None,
                };
            };
            if provider_entry.auth.oauth.is_none() {
                return AuthCheckResult {
                    status: "not_ready".to_string(),
                    provider: provider.to_string(),
                    reason: Some("credentials_not_configured".to_string()),
                    auth_type: None,
                };
            }
            let refreshed =
                match get_provider_credential(provider, registry, auth_path, true, None).await {
                    Ok(Some(refreshed)) => refreshed,
                    Ok(None) => {
                        return AuthCheckResult {
                            status: "not_ready".to_string(),
                            provider: provider.to_string(),
                            reason: Some("credentials_not_configured".to_string()),
                            auth_type: None,
                        }
                    }
                    Err(_) => {
                        return AuthCheckResult {
                            status: "invalid".to_string(),
                            provider: provider.to_string(),
                            reason: Some("invalid_state".to_string()),
                            auth_type: None,
                        }
                    }
                };
            return auth_result_from_source(
                provider,
                source_from_stored(
                    registry.get_provider(provider).as_ref(),
                    &refreshed,
                    config.get_provider(provider),
                ),
            );
        }

        return auth_result_from_source(
            provider,
            source_from_stored(
                registry.get_provider(provider).as_ref(),
                &credential,
                config.get_provider(provider),
            ),
        );
    }

    if config
        .get_provider(provider)
        .and_then(|provider| provider.api_key.as_deref())
        .is_some()
    {
        return auth_result_from_source(
            provider,
            configured_api_key_status(config.get_provider(provider)),
        );
    }
    if let Some(source) = source_from_facade(provider, registry) {
        return auth_result_from_source(provider, Some(source));
    }

    auth_result_from_source(
        provider,
        configured_api_key_status(config.get_provider(provider)),
    )
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

/// `pi auth` dispatcher. Returns true when the args were an auth command.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub async fn handle_auth_command(args: &[String]) -> bool {
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
        let credential = match get_provider_credential(
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
        )
        .await
        {
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
        let target = match resolve_auth_target(
            validated.provider.as_deref(),
            validated.model.as_deref(),
            &registry,
        ) {
            Ok(target) => target,
            Err(message) => {
                eprintln!("Error: {message}");
                std::process::exit(2);
            }
        };
        let mut result = check_provider_auth_with_options(
            &target.provider,
            &registry,
            &auth_path,
            !command.no_refresh,
        )
        .await;
        let mut credential_value: Option<String> = None;
        if command.credentials && result.status == "ready" {
            match get_provider_credential(
                &result.provider,
                &registry,
                &auth_path,
                !command.no_refresh,
                None,
            )
            .await
            {
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;

    use pi_ai::auth::{
        ApiKeyCredential, AuthContext, Credential as AiCredential, CredentialStore,
        InMemoryCredentialStore, OAuthCredential,
    };

    fn test_registry(
        env_values: &[(&str, &str)],
        credentials: Option<Arc<dyn CredentialStore>>,
    ) -> ModelRegistry {
        let env_values: Arc<BTreeMap<String, String>> = Arc::new(
            env_values
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        );
        let auth_context = AuthContext {
            env: {
                let env_values = Arc::clone(&env_values);
                Arc::new(move |name| env_values.get(name).cloned())
            },
            file_exists: Arc::new(|_| false),
        };
        let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions {
            auth_context: Some(auth_context),
            credentials,
            ..Default::default()
        });
        ModelRegistry::new(models, ModelConfig::default())
    }

    fn auth_fixture_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pi-auth-command-{label}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cleanup_auth_fixture(path: &Path) {
        let _ = fs::remove_file(path);
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(lock_path);
    }

    fn config_only_provider() -> ModelConfig {
        ModelConfig::from_value(serde_json::json!({
            "providers": {
                "configured-only": {
                    "baseUrl": "https://configured.example/v1",
                    "api": "openai-responses",
                    "apiKey": "configured-secret",
                    "models": [{ "id": "configured-model", "reasoning": false }]
                }
            }
        }))
        .unwrap()
    }

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

    #[tokio::test]
    async fn auth_check_uses_environment_credential_without_exposing_it() {
        let registry = test_registry(&[("OPENAI_API_KEY", "env-secret")], None);
        let config = ModelConfig::default();
        let path = auth_fixture_path("env");

        let result =
            check_provider_auth_with_options_and_config("openai", &registry, &path, false, &config)
                .await;

        assert_eq!(result.status, "ready");
        assert_eq!(result.provider, "openai");
        assert_eq!(result.auth_type.as_deref(), Some("api_key"));
        assert_eq!(result.reason, None);
        let source = source_from_facade("openai", &registry).expect("environment auth source");
        assert_eq!(source.source.as_deref(), Some("OPENAI_API_KEY"));

        let credential = get_provider_credential("openai", &registry, &path, false, None)
            .await
            .unwrap()
            .expect("environment credential");
        assert_eq!(
            auth_credential_value(&credential).as_deref(),
            Some("env-secret")
        );
        assert!(!format!("{result:?}").contains("env-secret"));
        cleanup_auth_fixture(&path);
    }

    #[tokio::test]
    async fn auth_check_uses_stored_api_key_and_reports_metadata_only() {
        let store = Arc::new(InMemoryCredentialStore::new());
        store.modify("openai", &|_| {
            Some(AiCredential::ApiKey(ApiKeyCredential {
                key: Some("stored-secret".to_string()),
                env: None,
            }))
        });
        let registry = test_registry(&[], Some(store.clone()));
        let config = ModelConfig::default();
        let path = auth_fixture_path("stored-api");

        let result =
            check_provider_auth_with_options_and_config("openai", &registry, &path, false, &config)
                .await;

        assert_eq!(result.status, "ready");
        assert_eq!(result.auth_type.as_deref(), Some("api_key"));
        let source = source_from_facade("openai", &registry).expect("stored auth source");
        assert_eq!(source.source.as_deref(), Some("stored credential"));
        let credential = get_provider_credential("openai", &registry, &path, false, None)
            .await
            .unwrap()
            .expect("stored credential");
        assert_eq!(
            auth_credential_value(&credential).as_deref(),
            Some("stored-secret")
        );
        assert!(!format!("{result:?}").contains("stored-secret"));
        cleanup_auth_fixture(&path);
    }

    #[tokio::test]
    async fn auth_check_uses_stored_oauth_credential() {
        let store = Arc::new(InMemoryCredentialStore::new());
        store.modify("openai-codex", &|_| {
            Some(AiCredential::OAuth(OAuthCredential {
                access: "oauth-access-secret".to_string(),
                refresh: "oauth-refresh-secret".to_string(),
                expires: u64::MAX,
                extra: BTreeMap::new(),
            }))
        });
        let registry = test_registry(&[], Some(store));
        let config = ModelConfig::default();
        let path = auth_fixture_path("stored-oauth");

        let result = check_provider_auth_with_options_and_config(
            "openai-codex",
            &registry,
            &path,
            false,
            &config,
        )
        .await;

        assert_eq!(result.status, "ready");
        assert_eq!(result.auth_type.as_deref(), Some("oauth"));
        let source = source_from_facade("openai-codex", &registry).expect("OAuth source");
        assert_eq!(source.source.as_deref(), Some("OAuth"));
        assert!(!format!("{result:?}").contains("oauth-access-secret"));
        assert!(!format!("{result:?}").contains("oauth-refresh-secret"));
        cleanup_auth_fixture(&path);
    }

    #[tokio::test]
    async fn auth_check_reports_missing_credentials_truthfully() {
        let registry = test_registry(&[], None);
        let config = ModelConfig::default();
        let path = auth_fixture_path("missing");

        let result =
            check_provider_auth_with_options_and_config("openai", &registry, &path, false, &config)
                .await;

        assert_eq!(result.status, "not_ready");
        assert_eq!(result.provider, "openai");
        assert_eq!(result.reason.as_deref(), Some("credentials_not_configured"));
        assert_eq!(result.auth_type, None);
        cleanup_auth_fixture(&path);
    }

    #[tokio::test]
    async fn auth_check_reports_unknown_provider_without_secret_data() {
        let registry = test_registry(&[], None);
        let config = ModelConfig::default();
        let path = auth_fixture_path("unknown-provider");

        let result = check_provider_auth_with_options_and_config(
            "not-a-provider",
            &registry,
            &path,
            false,
            &config,
        )
        .await;

        assert_eq!(result.status, "not_ready");
        assert_eq!(result.provider, "not-a-provider");
        assert_eq!(result.reason.as_deref(), Some("provider_not_found"));
        assert!(!format!("{result:?}").contains("secret"));
        cleanup_auth_fixture(&path);
    }

    #[test]
    fn auth_model_resolution_reports_unknown_model() {
        let registry = test_registry(&[], None);
        let config = ModelConfig::default();
        let error =
            resolve_auth_target_with_config(None, Some("not-a-real-model"), &registry, &config)
                .unwrap_err();
        assert!(error.contains("not-a-real-model"), "{error}");
    }

    #[tokio::test]
    async fn auth_check_resolves_models_json_provider_and_model() {
        let registry = test_registry(&[], None);
        let config = config_only_provider();
        let path = auth_fixture_path("configured-only");

        let target = resolve_auth_target_with_config(
            None,
            Some("configured-only/configured-model"),
            &registry,
            &config,
        )
        .expect("configured model target");
        assert_eq!(target.provider, "configured-only");
        assert_eq!(
            target.model.as_ref().map(|model| model.id.as_str()),
            Some("configured-model")
        );

        let result = check_provider_auth_with_options_and_config(
            &target.provider,
            &registry,
            &path,
            false,
            &config,
        )
        .await;
        assert_eq!(result.status, "ready");
        assert_eq!(result.auth_type.as_deref(), Some("api_key"));
        assert_eq!(
            configured_api_key_status(config.get_provider("configured-only"))
                .and_then(|source| source.source),
            Some("configured API key".to_string())
        );
        cleanup_auth_fixture(&path);
    }

    #[test]
    fn auth_result_credential_value_extracts_bearer_header_only() {
        let auth = pi_ai::auth::AuthResult {
            auth: pi_ai::auth::ModelAuth {
                api_key: None,
                headers: Some(BTreeMap::from([(
                    "Authorization".to_string(),
                    Some("Bearer header-secret".to_string()),
                )])),
                base_url: None,
            },
            env: None,
            source: Some("test".to_string()),
        };
        assert_eq!(
            auth_result_credential_value(&auth).as_deref(),
            Some("header-secret")
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
