//! Non-interactive run path — the `pi -p` / `pi <message>` flow. Wires the
//! provider (faux for tests; other providers as they are ported), the agent
//! loop, and session persistence.
//!
//! Provider/model resolution order (1:1 with upstream `findInitialModel` for
//! the one-shot path, plus the port's documented env surface): explicit
//! CLI/env values → paired authenticated settings default → first
//! authenticated provider default → the legacy `google` fallback for the
//! downstream no-credentials diagnostic.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi_agent::fs::StdFileSystem;
use pi_agent::harness::compaction::{
    compact, estimate_context_tokens, prepare_compaction, should_compact, CompactionSettings,
};
use pi_agent::harness::SimpleModels;
use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::session::context::{build_session_context, SessionContextBuildOptions};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::types::{Entry, EntryNoStats, SessionMetadata};
use pi_agent::session::{CreateOptions, ForkOptions, JsonlSessionRepo, Session};
use pi_agent::tools::image::{
    detect_supported_image_mime_type, process_image, ProcessImageOptions,
};
use pi_agent::types::AgentMessage;
use pi_ai::types::{ContentBlock, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::model_resolver::{
    find_initial_model, resolve_cli_model, resolve_model_scope_from_models, InitialModelOptions,
    ModelScopeDiagnostic, RegistrySnapshot, ScopedModel, DEFAULT_THINKING_LEVEL,
};
use crate::core::settings::SettingsManager;

/// Dispatch an explicitly gated experimental process command. Keeping this
/// entry point beside the normal run path makes the binary's lifecycle/error
/// handling uniform while the experimental implementation remains isolated.
pub async fn run_experimental_command(
    command: crate::core::experimental::ExperimentalCommand,
) -> Result<(), String> {
    match command {
        crate::core::experimental::ExperimentalCommand::Server { .. } => {
            crate::core::experimental::run_server(command).await
        }
        crate::core::experimental::ExperimentalCommand::Client { .. } => {
            crate::core::experimental::run_client(command).await
        }
    }
}

/// Provider stream function: `(model, context) -> event stream`.
pub type StreamFn = Arc<
    dyn Fn(&pi_ai::model::Model, &pi_ai::types::Context) -> pi_ai::AssistantMessageEventStream
        + Send
        + Sync,
>;

pub struct RunOutcome {
    pub final_text: String,
    pub session_path: Option<String>,
}

/// Invalidate external extension processes whenever the one-shot mode exits,
/// including early errors after extension loading.
struct RunExtensionGuard(Arc<crate::core::extensions::ExtensionRunner>);

impl Drop for RunExtensionGuard {
    fn drop(&mut self) {
        let _ = self.0.emit_session_shutdown("quit");
        self.0.invalidate(Some("print mode shutdown"));
    }
}

fn normalize_run_provider(provider: String) -> String {
    if provider.eq_ignore_ascii_case("faux") {
        "faux".to_string()
    } else if provider.eq_ignore_ascii_case(crate::core::llama::LLAMA_PROVIDER_ID) {
        crate::core::llama::LLAMA_PROVIDER_ID.to_string()
    } else {
        provider
    }
}

/// Resolve a provider spelling to the canonical registration id. Model
/// resolution is case-insensitive upstream, including models.json/native
/// providers that are unavailable to the initial built-in-only bootstrap.
pub(crate) fn canonicalize_registered_provider(
    models: &pi_ai::models::Models,
    provider: &str,
) -> String {
    models
        .get_providers()
        .into_iter()
        .find(|candidate| candidate.id.eq_ignore_ascii_case(provider))
        .map(|candidate| candidate.id)
        .unwrap_or_else(|| provider.to_string())
}

pub(crate) fn unknown_provider_error(provider: &str) -> String {
    format!("Unknown provider \"{provider}\". Use --list-models to see available providers/models.")
}

fn nonempty_value(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// Build the per-turn stream function that carries turn-scoped options into
/// `stream_simple`.
///
/// The bare `StreamFn` path only passes base `StreamOptions`, so without
/// this the harness-selected thinking level never reaches provider requests
/// (upstream sends per-turn reasoning). Only reasoning is overlaid here;
/// auth, transport, telemetry, session affinity, and abort keep flowing
/// through the captured base options and facade state exactly as before.
pub(crate) fn stream_fn_with_reasoning(
    models: pi_ai::models::Models,
    base: pi_ai::types::SimpleStreamOptions,
) -> pi_agent::StreamFnWithOptions {
    Arc::new(move |model, ctx, turn_options| {
        let mut merged = base.clone();
        if turn_options.reasoning.is_some() {
            merged.reasoning = turn_options.reasoning;
        }
        models.stream_simple(model, ctx, Some(&merged))
    })
}

/// Resolved startup thinking level plus an optional invalid-environment
/// warning. Precedence mirrors provider/model selection: explicit CLI, then
/// the model-hint scope, then `PI_REASONING_LEVEL`, then the settings
/// default, then the builtin default. An invalid environment value warns in
/// the `--thinking` message shape and falls through instead of failing the
/// run.
pub(crate) struct ResolvedThinkingLevel {
    pub level: String,
    pub warning: Option<String>,
}

pub(crate) fn resolve_requested_thinking_level(
    cli: Option<&str>,
    selected: Option<&str>,
    env: Option<&str>,
    settings_default: Option<&str>,
    builtin_default: &str,
) -> ResolvedThinkingLevel {
    let mut warning = None;
    let level = nonempty_value(cli)
        .or_else(|| nonempty_value(selected))
        .or_else(|| match nonempty_value(env) {
            Some(value) if crate::args::VALID_THINKING_LEVELS.contains(&value) => Some(value),
            Some(value) => {
                warning = Some(format!(
                    "Invalid PI_REASONING_LEVEL \"{value}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                ));
                None
            }
            None => None,
        })
        .or_else(|| nonempty_value(settings_default))
        .unwrap_or(builtin_default)
        .to_string();
    ResolvedThinkingLevel { level, warning }
}

/// Resolve the model-scope source using the pinned CLI precedence: an
/// explicit `--models` value wins, while omitted CLI scope inherits the
/// persisted `enabledModels` setting.
fn effective_model_patterns(args: &Args, settings: &SettingsManager) -> Vec<String> {
    if !args.models.is_empty() {
        args.models.clone()
    } else {
        settings.get_enabled_models().unwrap_or_default()
    }
}

/// Resolve the effective model scope once for every startup mode. The model
/// list is supplied by the caller after its native providers/models.json have
/// been registered, while the CLI/settings precedence remains shared.
pub(crate) fn resolve_effective_model_scope(
    args: &Args,
    settings: &SettingsManager,
    models: &[pi_ai::model::Model],
) -> (Vec<ScopedModel>, Vec<ModelScopeDiagnostic>) {
    let patterns = effective_model_patterns(args, settings);
    if patterns.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        resolve_model_scope_from_models(&patterns, models)
    }
}

/// Provider resolution for the run path: explicit CLI/env → authenticated
/// saved default → first authenticated provider default → settings/`google`
/// fallback. The final fallback preserves the existing downstream auth
/// diagnostic when no credentials are available.
pub fn resolve_run_provider(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    settings: &SettingsManager,
) -> String {
    if let Some(provider) = nonempty_value(cli_provider)
        .map(str::to_owned)
        .or_else(|| config::nonempty_env_value(config::env(config::ENV_PROVIDER)))
    {
        return normalize_run_provider(provider);
    }

    let model_reference = nonempty_value(cli_model)
        .map(str::to_owned)
        .or_else(|| config::nonempty_env_value(config::env(config::ENV_MODEL)));
    if let Some(prefix) = model_reference
        .as_deref()
        .and_then(|model| model.split_once('/').map(|(provider, _)| provider))
    {
        if prefix.eq_ignore_ascii_case("faux") {
            return "faux".to_string();
        }
        let builtins = crate::core::model_registry::builtin_models();
        if let Some(provider) = builtins
            .get_providers()
            .into_iter()
            .find(|provider| provider.id.eq_ignore_ascii_case(prefix))
        {
            return provider.id;
        }
    }

    let env_model = config::nonempty_env_value(config::env(config::ENV_MODEL));
    let models = crate::core::model_registry::builtin_models();
    let snapshot = RegistrySnapshot::from_models(&models);
    let initial = find_initial_model(InitialModelOptions {
        cli_provider: None,
        cli_model: None,
        env_provider: None,
        env_model: env_model.as_deref(),
        scoped_models: &[],
        is_continuing: false,
        default_provider: settings.get_default_provider(),
        default_model_id: settings.get_default_model(),
        default_thinking_level: settings.get_default_thinking_level(),
        registry: &snapshot,
    })
    .ok()
    .and_then(|result| result.model)
    .map(|model| model.provider);

    let provider = initial
        .or_else(|| settings.get_default_provider().map(str::to_owned))
        .unwrap_or_else(|| "google".to_string());
    normalize_run_provider(provider)
}

/// Model-hint resolution for the run path: CLI → env → settings → None.
///
/// `apply_settings_default` gates the settings stage: upstream pairs
/// settings `defaultProvider`+`defaultModel` as a unit and resolves models
/// from the provider's own scope once an explicit provider source (CLI/env)
/// is present, so the settings default model must not leak into that scope.
pub fn resolve_run_model(
    cli_model: Option<&str>,
    settings: &SettingsManager,
    apply_settings_default: bool,
    resolved_provider: Option<&str>,
) -> Option<String> {
    if let Some(model) = nonempty_value(cli_model)
        .map(str::to_owned)
        .or_else(|| config::nonempty_env_value(config::env(config::ENV_MODEL)))
    {
        return Some(model);
    }

    if !apply_settings_default {
        return None;
    }

    // Settings defaults are a pair. Known built-in providers are only
    // eligible when their auth is configured; otherwise an authenticated
    // fallback provider selected by `resolve_run_provider` must not receive
    // the stale saved model id. Unknown providers remain compatible with the
    // models.json/native-provider boundary and are resolved by the caller's
    // complete catalog.
    let (Some(provider), Some(model)) = (
        settings.get_default_provider(),
        settings.get_default_model(),
    ) else {
        return None;
    };
    if let Some(resolved_provider) = resolved_provider {
        if !provider.eq_ignore_ascii_case(resolved_provider) {
            return None;
        }
    }
    if provider == "faux" {
        return Some(model.to_string());
    }
    let models = crate::core::model_registry::builtin_models();
    if models.get_provider(provider).is_none() || models.check_auth(provider).is_some() {
        Some(model.to_string())
    } else {
        None
    }
}

/// True when an explicit provider source (CLI flag or PI_PROVIDER env) is in
/// play; settings defaults then apply only to the model stage at most.
pub fn has_explicit_provider(cli_provider: Option<&str>) -> bool {
    nonempty_value(cli_provider).is_some()
        || config::env(config::ENV_PROVIDER).is_some_and(|provider| !provider.is_empty())
}

/// Reject an implicit catalog default when its provider has no usable
/// credentials. The full run path uses `find_initial_model`, but the
/// interactive/JSON/RPC callers resolve their provider and concrete model in
/// separate stages. Without this guard those callers select Google's first
/// catalog model in a clean environment and only fail later with the
/// misleading `Provider is not configured` error.
///
/// An explicit model hint is intentionally allowed through: callers may pair
/// it with a request-scoped `--api-key`/`PI_KEY`, which is attached after
/// model resolution and is never persisted.
pub fn require_authenticated_implicit_model(
    models: &pi_ai::models::Models,
    provider: &str,
    model_hint: Option<&str>,
) -> Result<(), String> {
    if model_hint.is_none()
        && models.get_provider(provider).is_some()
        && models.check_auth(provider).is_none()
    {
        return Err(crate::core::auth_guidance::format_no_models_available_message());
    }
    Ok(())
}

/// Return the request-scoped key using the same truthiness rule as upstream
/// `main.ts`: an empty CLI value does not override the environment, and an
/// empty environment value is not usable authentication.
fn request_api_key(args: &Args, env_api_key: Option<String>) -> Option<String> {
    args.api_key
        .as_deref()
        .filter(|api_key| !api_key.is_empty())
        .map(str::to_owned)
        .or_else(|| env_api_key.filter(|api_key| !api_key.is_empty()))
}

/// Resolve the two retry layers shared by print, JSON, interactive, and RPC:
/// the agent-level retry policy and the provider-request transport limits.
pub(crate) fn retry_policy_from_settings(settings: &SettingsManager) -> pi_ai::utils::RetryPolicy {
    let (enabled, max_retries, base_delay_ms) = settings.get_retry_settings();
    pi_ai::utils::RetryPolicy {
        enabled,
        max_retries: u32::try_from(max_retries).unwrap_or(u32::MAX),
        base_delay_ms,
    }
}

pub(crate) fn stream_options_from_settings(
    settings: &SettingsManager,
    api_key: Option<String>,
) -> pi_ai::types::StreamOptions {
    let (provider_timeout_ms, provider_max_retries, max_retry_delay_ms) =
        settings.get_provider_retry_settings();
    let idle_timeout_ms = settings.get_http_idle_timeout_ms().unwrap_or(300_000);
    let effective_idle_timeout_ms = if idle_timeout_ms == 0 {
        i32::MAX as u64
    } else {
        idle_timeout_ms
    };
    pi_ai::types::StreamOptions {
        base: pi_ai::types::ProviderRequestOptions {
            api_key,
            timeout_ms: Some(provider_timeout_ms.unwrap_or(effective_idle_timeout_ms)),
            max_retries: provider_max_retries
                .map(|retries| u32::try_from(retries).unwrap_or(u32::MAX)),
            max_retry_delay_ms: Some(max_retry_delay_ms),
            ..Default::default()
        },
        transport: Some(settings.get_transport().to_string()),
        websocket_connect_timeout_ms: settings.get_websocket_connect_timeout_ms().ok().flatten(),
        ..Default::default()
    }
}

/// Read the global `defaultProjectTrust` setting (allow/deny/ask) without
/// loading project settings (which are themselves trust-gated).
fn settings_default_project_trust(agent_dir: &std::path::Path) -> Option<String> {
    let path = agent_dir.join("settings.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let content = crate::core::settings::strip_bom(&raw);
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    value
        .get("defaultProjectTrust")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn project_trust_override(args: &Args) -> Option<bool> {
    if args.approve {
        Some(true)
    } else if args.no_approve {
        Some(false)
    } else {
        None
    }
}

fn project_trust_prompt(cwd: &str) -> String {
    format!(
        "Trust project folder?\n{cwd}\n\nThis allows pi to load .pi settings and resources, install missing project packages, and execute project extensions."
    )
}

fn extension_project_trust_decision(
    runner: &crate::core::extensions::ExtensionRunner,
    cwd: &str,
    trust_store: &crate::core::project_trust::ProjectTrustStore,
) -> Option<bool> {
    let output = crate::core::extensions::emit_project_trust_event(
        runner,
        &serde_json::json!({"type": "project_trust", "cwd": cwd}),
    );
    for error in output.errors {
        eprintln!(
            "Extension \"{}\" project_trust error: {}",
            error.extension_path, error.error
        );
    }
    let result = output.result?;
    let trusted = result
        .get("trusted")
        .or_else(|| result.get("result"))
        .and_then(serde_json::Value::as_str)?;
    let trusted = match trusted {
        "yes" => true,
        "no" => false,
        _ => return None,
    };
    if result.get("remember").and_then(serde_json::Value::as_bool) == Some(true) {
        if let Err(error) = trust_store.try_set(cwd, Some(trusted)) {
            eprintln!("Could not save project trust: {error}");
            return Some(false);
        }
    }
    Some(trusted)
}

fn resolve_project_trust_without_prompt(
    cwd: &str,
    agent_dir: &std::path::Path,
    trust_override: Option<bool>,
    extension_runner: Option<&crate::core::extensions::ExtensionRunner>,
) -> Option<bool> {
    if let Some(override_value) = trust_override {
        return Some(override_value);
    }
    if !crate::core::project_trust::has_trust_requiring_project_resources(cwd) {
        return Some(true);
    }
    let trust_store =
        crate::core::project_trust::ProjectTrustStore::new(&agent_dir.display().to_string());
    if let Some(runner) = extension_runner {
        if let Some(decision) = extension_project_trust_decision(runner, cwd, &trust_store) {
            return Some(decision);
        }
    }
    match trust_store.try_get(cwd) {
        Ok(Some(saved)) => Some(saved),
        Ok(None) => match settings_default_project_trust(agent_dir).as_deref() {
            Some("always") => Some(true),
            Some("never") => Some(false),
            _ => None,
        },
        Err(error) => {
            // A malformed or temporarily inaccessible trust file must not
            // abort startup before the user can repair it. Fail closed and
            // expose the exact storage failure; the interactive selector can
            // then retry/save with its Result path.
            eprintln!("Could not read project trust: {error}");
            Some(false)
        }
    }
}

fn trust_bootstrap_mode(args: &Args, has_ui: bool) -> &'static str {
    if has_ui {
        "interactive"
    } else {
        match args.mode.as_deref() {
            Some("json") => "json",
            Some("rpc") => "rpc",
            _ => "print",
        }
    }
}

fn resolve_mode_project_trust_without_prompt(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    has_ui: bool,
) -> Option<bool> {
    let trust_override = project_trust_override(args);
    if trust_override.is_some()
        || !crate::core::project_trust::has_trust_requiring_project_resources(cwd)
    {
        return resolve_project_trust_without_prompt(cwd, agent_dir, trust_override, None);
    }

    // The bootstrap SettingsManager is deliberately untrusted. Its extension
    // paths therefore contain only global settings, and the scoped loader
    // excludes cwd/.pi/extensions while retaining explicit CLI paths.
    let bootstrap_settings = SettingsManager::create(
        cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    let loaded = crate::core::extensions::load_for_project_trust(
        args,
        &bootstrap_settings,
        cwd,
        &agent_dir.display().to_string(),
        trust_bootstrap_mode(args, has_ui),
        has_ui,
        args.name.clone(),
        args.thinking.as_deref().unwrap_or("medium"),
    );
    for error in &loaded.errors {
        eprintln!(
            "Failed to load extension \"{}\": {}",
            error.path, error.error
        );
    }
    let decision =
        resolve_project_trust_without_prompt(cwd, agent_dir, trust_override, Some(&loaded.runner));
    loaded
        .runner
        .invalidate(Some("project trust bootstrap complete"));
    decision
}

/// Create settings for a mode after applying the upstream project-trust
/// precedence: explicit CLI override, saved directory decision, global
/// `defaultProjectTrust`, then an interactive startup prompt when UI exists.
/// Headless modes deliberately treat an unresolved `ask` decision as
/// untrusted, so project resources cannot execute merely because a mode
/// bypassed the normal interactive startup.
pub fn create_settings_with_project_trust(
    cwd: &str,
    agent_dir: &std::path::Path,
    trust_override: Option<bool>,
    _has_ui: bool,
) -> SettingsManager {
    let project_trusted =
        resolve_project_trust_without_prompt(cwd, agent_dir, trust_override, None).unwrap_or(false);
    SettingsManager::create(
        cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions { project_trusted },
    )
}

/// Mode entry points share one trust gate so interactive, JSON, and RPC do
/// not accidentally load project-local resources with their default settings.
pub fn create_mode_settings(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    has_ui: bool,
) -> SettingsManager {
    let project_trusted =
        resolve_mode_project_trust_without_prompt(args, cwd, agent_dir, has_ui).unwrap_or(false);
    SettingsManager::create(
        cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions { project_trusted },
    )
}

/// Interactive startup trust resolution uses the same pi-tui selector as the
/// other startup dialogs. Cancellation and EOF fail closed; durable and
/// session-only choices retain the ordered upstream option semantics.
pub async fn create_interactive_mode_settings(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
) -> Result<SettingsManager, String> {
    let project_trusted =
        match resolve_mode_project_trust_without_prompt(args, cwd, agent_dir, true) {
            Some(project_trusted) => project_trusted,
            None => {
                let bootstrap_settings = SettingsManager::create(
                    cwd,
                    &agent_dir.display().to_string(),
                    crate::core::settings::SettingsManagerCreateOptions {
                        project_trusted: false,
                    },
                );
                let options = crate::core::project_trust::get_project_trust_options(cwd, true);
                let startup_options = options
                    .into_iter()
                    .map(|option| crate::interactive::startup::StartupOption {
                        label: option.label.clone(),
                        value: option,
                    })
                    .collect();
                let selected = crate::interactive::startup::show_startup_selector(
                    &bootstrap_settings,
                    project_trust_prompt(cwd),
                    startup_options,
                )
                .await?;
                let Some(selected) = selected else {
                    return Ok(SettingsManager::create(
                        cwd,
                        &agent_dir.display().to_string(),
                        crate::core::settings::SettingsManagerCreateOptions {
                            project_trusted: false,
                        },
                    ));
                };
                if !selected.updates.is_empty() {
                    let trust_store = crate::core::project_trust::ProjectTrustStore::new(
                        &agent_dir.display().to_string(),
                    );
                    if let Err(error) = trust_store.try_set_many(&selected.updates) {
                        eprintln!("Could not save project trust: {error}");
                        return Ok(SettingsManager::create(
                            cwd,
                            &agent_dir.display().to_string(),
                            crate::core::settings::SettingsManagerCreateOptions {
                                project_trusted: false,
                            },
                        ));
                    }
                }
                selected.trusted
            }
        };
    Ok(SettingsManager::create(
        cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions { project_trusted },
    ))
}

/// Run the upstream-gated first-run setup before normal interactive startup.
/// This owns a temporary settings manager so setup can never depend on a
/// provider turn or on the later project-resource trust prompt.
pub async fn run_first_time_setup_if_needed(
    cwd: &str,
    agent_dir: &std::path::Path,
) -> Result<bool, String> {
    let settings_path = agent_dir.join("settings.json");
    if !crate::interactive::startup::should_run_first_time_setup(&settings_path) {
        return Ok(false);
    }
    let mut settings = SettingsManager::create(
        cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions {
            project_trusted: false,
        },
    );
    crate::interactive::startup::show_first_time_setup(&mut settings).await?;
    Ok(true)
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub async fn run(args: &Args) -> Result<RunOutcome, String> {
    let cwd = config::cwd();
    let agent_dir = config::get_agent_dir();
    let mut settings = create_mode_settings(args, &cwd, &agent_dir, false);
    // Surface settings load errors as diagnostics (never silently ignore a
    // malformed settings.json that the user expects to take effect).
    let settings_errors = settings.drain_errors();
    if let Some(first) = settings_errors.first() {
        tracing::warn!(
            scope = ?first.scope,
            path = ?first.path,
            error = %first.error,
            "settings load error; continuing with defaults"
        );
    }
    // The pinned CLI treats an explicit --models value as an override and
    // otherwise falls back to the persisted enabledModels scope. Args uses an
    // empty Vec for an omitted flag, so resolve that distinction here.
    let model_patterns = effective_model_patterns(args, &settings);

    let loaded_extensions = crate::core::extensions::load_for_mode(
        args,
        &settings,
        &cwd,
        &agent_dir.to_string_lossy(),
        "print",
        false,
        args.name.clone(),
        settings
            .get_default_thinking_level()
            .unwrap_or("medium")
            .to_string(),
    );
    for error in &loaded_extensions.errors {
        tracing::warn!(path = %error.path, error = %error.error, "failed to load extension");
    }
    let _extension_guard = RunExtensionGuard(loaded_extensions.runner.clone());

    let mut provider =
        resolve_run_provider(args.provider.as_deref(), args.model.as_deref(), &settings);
    let model_hint = resolve_run_model(
        args.model.as_deref(),
        &settings,
        !has_explicit_provider(args.provider.as_deref()),
        Some(&provider),
    );

    // Build the model + stream function for the selected provider. Real
    // providers route through the pi-ai Models facade (catalog-backed model
    // resolution + auth application + api dispatch). `faux` keeps its
    // scripted path for tests.
    let mut selected_provider_uses_oauth = false;
    let mut selected_thinking_level: Option<String> = None;
    let (mut model, stream_fn, summary_stream_fn, stream_fn_with_options): (
        pi_ai::model::Model,
        StreamFn,
        StreamFn,
        Option<pi_agent::StreamFnWithOptions>,
    ) = if provider == "faux" {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let core = crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        crate::core::extensions::register_loaded_native_providers(&models, &loaded_extensions)
            .map_err(|error| format!("register extension providers: {error}"))?;
        let model = if let Some(hint) = model_hint.as_deref() {
            let resolved = resolve_cli_model(
                args.provider.as_deref(),
                Some(hint),
                args.thinking.as_deref(),
                &core.models,
            );
            if let Some(warning) = resolved.warning {
                eprintln!("Warning: {warning}");
            }
            if let Some(error) = resolved.error {
                return Err(error);
            }
            selected_thinking_level = resolved.thinking_level;
            resolved
                .model
                .ok_or_else(|| format!("unknown faux model {hint:?}"))?
        } else if !model_patterns.is_empty() {
            let (scoped_models, diagnostics) =
                resolve_model_scope_from_models(&model_patterns, &core.models);
            for diagnostic in diagnostics {
                eprintln!("Warning: {}", diagnostic.message);
            }
            let scoped = scoped_models
                .into_iter()
                .next()
                .ok_or_else(|| "No models match the requested --models patterns".to_string())?;
            selected_thinking_level = scoped.thinking_level;
            scoped.model
        } else {
            core.models
                .first()
                .cloned()
                .ok_or_else(|| "no faux model".to_string())?
        };
        // Queue one scripted faux response per prompt so sequential
        // print-mode turns (one assistant turn per positional message,
        // upstream `runPrintMode`) each pop a reply.
        let mut prompts = Vec::new();
        let stdin_content = args.stdin_content.as_deref().unwrap_or_default();
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_prompt = format!("{stdin_content}{first_message}");
        if !initial_prompt.is_empty() {
            prompts.push(initial_prompt);
        } else if args.messages.is_empty() {
            prompts.push("Hello from pi-rust".to_string());
        }
        prompts.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .cloned(),
        );
        let responses: Vec<pi_ai::providers::FauxResponseStep> = prompts
            .into_iter()
            .map(|text| {
                pi_ai::providers::FauxResponseStep::Message(
                    pi_ai::providers::faux_assistant_message(
                        vec![pi_ai::types::ContentBlock::text(format!(
                            "faux response to: {text}"
                        ))],
                        pi_ai::providers::FauxAssistantOptions::default(),
                    ),
                )
            })
            .collect();
        core.set_responses(responses);
        let stream_models = models.clone();
        let stream_fn: StreamFn = Arc::new(move |model, ctx| {
            stream_models.stream(model, ctx, Some(&pi_ai::types::StreamOptions::default()))
        });
        // Keep compaction completions off the scripted user-response
        // queue. The real provider uses the same stream path for both
        // calls; faux is deliberately split so a summary cannot consume a
        // later print turn.
        let summary_core = pi_ai::providers::FauxProviderCore::new(
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        let summary_responses = (0..64)
            .map(|_| {
                pi_ai::providers::FauxResponseStep::Message(
                    pi_ai::providers::faux_assistant_message(
                        vec![pi_ai::types::ContentBlock::text("faux compaction summary")],
                        pi_ai::providers::FauxAssistantOptions::default(),
                    ),
                )
            })
            .collect();
        summary_core.set_responses(summary_responses);
        let summary_core = summary_core.clone();
        let summary_stream_fn: StreamFn =
            Arc::new(move |model, ctx| summary_core.stream(model, ctx, None));
        let stream_fn_with_options = Some(stream_fn_with_reasoning(
            models.clone(),
            pi_ai::types::SimpleStreamOptions::default(),
        ));
        (model, stream_fn, summary_stream_fn, stream_fn_with_options)
    } else {
        // models.json runtime merge: the registry overlays the bundled
        // catalog with ~/.pi/agent/models.json (upstream applyModelsJson).
        let models = {
            let models = crate::core::model_registry::builtin_models();
            let config = crate::core::model_config::ModelConfig::load(
                crate::core::model_config::models_json_path().as_deref(),
            );
            let registry = crate::core::model_registry::ModelRegistry::new(models, config);
            registry.into_models()
        };
        crate::core::extensions::register_loaded_native_providers(&models, &loaded_extensions)
            .map_err(|error| format!("register extension providers: {error}"))?;
        provider = canonicalize_registered_provider(&models, &provider);
        crate::core::model_runtime::register_llama_provider_if_selected(
            &models,
            &provider,
            !args.offline && !config::env_flag(config::ENV_OFFLINE),
        )
        .await?;
        if models.get_provider(&provider).is_none() {
            return Err(unknown_provider_error(&provider));
        }
        let env_provider = config::nonempty_env_value(config::env(config::ENV_PROVIDER));
        let env_model = config::nonempty_env_value(config::env(config::ENV_MODEL));
        let (scoped_models, scope_diagnostics) = if model_patterns.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            resolve_model_scope_from_models(&model_patterns, &models.get_models(None))
        };
        for diagnostic in scope_diagnostics {
            eprintln!("Warning: {}", diagnostic.message);
        }
        let snapshot = RegistrySnapshot::from_models(&models);
        let initial = find_initial_model(InitialModelOptions {
            cli_provider: nonempty_value(args.provider.as_deref()),
            cli_model: nonempty_value(args.model.as_deref()),
            env_provider: env_provider.as_deref(),
            env_model: env_model.as_deref(),
            scoped_models: &scoped_models,
            is_continuing: false,
            default_provider: settings.get_default_provider(),
            default_model_id: settings.get_default_model(),
            default_thinking_level: settings.get_default_thinking_level(),
            registry: &snapshot,
        })?;
        let model = initial
            .model
            .ok_or_else(crate::core::auth_guidance::format_no_models_available_message)?;
        selected_thinking_level = if initial.thinking_explicit {
            Some(initial.thinking_level)
        } else {
            None
        };
        provider = model.provider.clone();
        // A request-scoped --api-key/PI_KEY is valid auth for the selected
        // provider, but must remain non-persistent. Attach it only after
        // model resolution so `--model provider/id` cannot leak the key
        // onto the provisional provider chosen before resolution.
        if let Some(api_key) = request_api_key(args, config::env(config::ENV_KEY)) {
            models.set_runtime_api_key(provider.clone(), api_key);
        }
        selected_provider_uses_oauth = models
            .get_provider(&provider)
            .is_some_and(|registered| registered.auth.oauth.is_some());
        crate::core::model_runtime::refresh_provider_oauth_if_needed(&models, &provider).await?;
        // Stream options carry the explicit --api-key / PI_KEY (the facade
        // applies env-key auth when absent).
        let api_key = request_api_key(args, config::env(config::ENV_KEY));
        let stream_options = stream_options_from_settings(&settings, api_key);
        let models = models.clone();
        let with_options_models = models.clone();
        let stream_fn: StreamFn =
            Arc::new(move |_model, ctx| models.stream(_model, ctx, Some(&stream_options)));
        let summary_stream_fn = stream_fn.clone();
        let stream_fn_with_options = Some(stream_fn_with_reasoning(
            with_options_models,
            pi_ai::types::SimpleStreamOptions {
                base: stream_options_from_settings(&settings, None),
                ..Default::default()
            },
        ));
        (model, stream_fn, summary_stream_fn, stream_fn_with_options)
    };

    // Register built-in tools (bash/read/write/edit + ls/find/grep) unless
    // --no-tools or --no-builtin-tools.
    let mut tools: Vec<pi_agent::tools::AgentTool> = Vec::new();
    if should_register_builtin_tools(args) {
        tools.push(pi_agent::tools::bash_tool_with_options(
            cwd.clone(),
            settings.get_shell_command_prefix().map(str::to_string),
            settings.get_shell_path(),
        ));
        tools.push(pi_agent::tools::read_tool_with_options(
            cwd.clone(),
            ProcessImageOptions {
                auto_resize_images: settings.get_image_auto_resize(),
                ..Default::default()
            },
        ));
        tools.push(pi_agent::tools::write_tool(cwd.clone()));
        tools.push(pi_agent::tools::edit_tool(cwd.clone()));
        tools.push(crate::core::tools::ls_tool(cwd.clone()));
        tools.push(crate::core::tools::find_tool(cwd.clone()));
        tools.push(crate::core::tools::grep_tool(cwd.clone()));
    }
    crate::core::extensions::install_tools(
        &loaded_extensions,
        &mut tools,
        should_register_extension_tools(args),
    );
    tools = select_active_tools(args, &settings, tools);
    let extension_tool_definitions = loaded_extensions.runner.get_all_registered_tools();
    // Build the prompt from the same filtered vector that is handed to the
    // harness.  This keeps tool descriptions, tool-guidelines, and skill
    // visibility truthful when an allowlist, denylist, setting, or extension
    // changes the active set.
    let mut system_prompt = assemble_run_system_prompt_with_active_tools(
        args,
        &cwd,
        &agent_dir,
        &settings,
        &loaded_extensions.resources,
        &tools,
        &extension_tool_definitions,
    );
    loaded_extensions.host.set_model(
        serde_json::to_value(&model)
            .ok()
            .filter(|value| !value.is_null()),
    );
    let mut system_prompt_overridden = false;
    if let Some(patch) = loaded_extensions.runner.emit_before_agent_start(
        args.messages
            .first()
            .map(String::as_str)
            .unwrap_or_default(),
        None,
        &system_prompt,
        &serde_json::json!({}),
    ) {
        if let Some(updated) = patch
            .get("systemPrompt")
            .and_then(serde_json::Value::as_str)
        {
            system_prompt = updated.to_string();
            system_prompt_overridden = true;
        }
    }
    // `before_agent_start` may call the host's model/tool mutation APIs. Apply
    // those requests to the actual print harness inputs before it is created;
    // leaving them in ExtensionHostState would only update the callback
    // snapshot and would be invisible to the first real turn.
    let requested_changes = loaded_extensions.host.drain_requested_changes();
    if let Some(requested_model) = requested_changes.model {
        model = serde_json::from_value(requested_model)
            .map_err(|error| format!("extension requested invalid run model: {error}"))?;
        loaded_extensions.host.set_model(
            serde_json::to_value(&model)
                .ok()
                .filter(|value| !value.is_null()),
        );
    }
    if let Some(active_tool_names) = requested_changes.active_tools {
        let all_tool_values = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.tool.name,
                    "description": tool.tool.description,
                    "parameters": tool.tool.parameters,
                })
            })
            .collect::<Vec<_>>();
        let mut active_tool_names = active_tool_names
            .into_iter()
            .filter(|name| tools.iter().any(|tool| tool.tool.name == *name))
            .collect::<Vec<_>>();
        active_tool_names.dedup();
        tools.retain(|tool| active_tool_names.iter().any(|name| name == &tool.tool.name));
        let mut command_runner = loaded_extensions.runner.as_ref().clone();
        let commands = command_runner
            .get_registered_commands()
            .into_iter()
            .map(|command| {
                serde_json::json!({
                    "name": command.invocation_name,
                    "description": command.description,
                })
            })
            .collect();
        loaded_extensions
            .host
            .set_catalog(active_tool_names, all_tool_values, commands);
        if !system_prompt_overridden {
            system_prompt = assemble_run_system_prompt_with_active_tools(
                args,
                &cwd,
                &agent_dir,
                &settings,
                &loaded_extensions.resources,
                &tools,
                &extension_tool_definitions,
            );
        }
    }
    // Resolve the durable session before creating the harness. This is the
    // point where the CLI session selectors become observable: continue and
    // resume open the selected v4 file, fork creates a child whose parent is
    // the selected session, and a normal run creates a fresh file. Legacy
    // files are migrated before inventory so every selector sees one format.
    let (harness_session, durable_session_path) = prepare_run_session_with_settings(
        args,
        &cwd,
        Some(loaded_extensions.runner.as_ref()),
        Some(&settings),
        false,
    )
    .await?;
    let session_metadata = harness_session.get_metadata().await;
    let resolved_thinking = resolve_requested_thinking_level(
        args.thinking.as_deref(),
        selected_thinking_level.as_deref(),
        config::env_reasoning_level().as_deref(),
        settings.get_default_thinking_level(),
        DEFAULT_THINKING_LEVEL,
    );
    if let Some(warning) = &resolved_thinking.warning {
        eprintln!("Warning: {warning}");
    }
    let requested_thinking_level = resolved_thinking.level;
    let thinking_level = pi_ai::model::clamp_thinking_level(
        &model,
        pi_ai::types::ModelThinkingLevel::from_effort_str(&requested_thinking_level),
    );
    let _session_environment = crate::core::session_env::install(
        &session_metadata.id,
        &session_metadata.path,
        &provider,
        &model.name,
        thinking_level.as_str(),
    );
    let harness_tools = tools
        .iter()
        .map(HarnessTool::from_agent_tool)
        .collect::<Vec<_>>();
    let mut harness_options = AgentHarnessOptions::new(harness_session, model.clone());
    harness_options.stream_fn = Some(stream_fn);
    harness_options.stream_fn_with_options = stream_fn_with_options;
    harness_options.system_prompt = Some(system_prompt);
    harness_options.block_images = settings.get_block_images();
    harness_options.tool_result_image_options = Some(ProcessImageOptions {
        auto_resize_images: settings.get_image_auto_resize(),
        ..Default::default()
    });
    harness_options.thinking_level = Some(thinking_level);
    harness_options.tools = Some(harness_tools);
    harness_options.retry = Some(retry_policy_from_settings(&settings));
    harness_options.stream_options = Some(pi_ai::types::SimpleStreamOptions {
        base: stream_options_from_settings(&settings, None),
        ..Default::default()
    });
    let (mut harness, _suspended) = AgentHarness::create(harness_options)
        .await
        .map_err(|error| format!("create agent harness: {error}"))?;

    // A resumed or forked session must rebuild the provider context before the
    // first new prompt. AgentHarness owns the live Agent state, while the
    // session file remains the source of truth for compaction boundaries and
    // derived model/tool settings.
    let existing_entries = harness
        .transcript()
        .await
        .map_err(|error| format!("read existing session transcript: {error}"))?;
    if !existing_entries.is_empty() {
        let context =
            build_session_context(&existing_entries, &SessionContextBuildOptions::default());
        harness
            .set_agent_messages(context.messages)
            .await
            .map_err(|error| format!("restore session context: {error}"))?;
    }

    // Expand `/template` prompt-template invocations in positional messages
    // (upstream `expandPromptTemplate`).
    let prompt_templates = load_prompt_templates_for_run_with_settings(
        args,
        &cwd,
        &agent_dir,
        &settings,
        &loaded_extensions.resources,
    );
    // Print mode prompts each positional message as its own sequential turn
    // (upstream `runPrintMode`: `for (const message of messages) { await
    // session.prompt(message); }`). Each turn's messages fold into the agent
    // context so a later prompt observes earlier turns.
    let mut all_messages: Vec<pi_agent::types::AgentMessage> = Vec::new();
    // The compaction harness consumes full entries (not just provider
    // messages), all sourced from the harness-owned main lane.
    let summarizer = SimpleModels::new({
        let summary_stream_fn = summary_stream_fn.clone();
        move |model, context, _options| {
            let stream = (summary_stream_fn)(model, context);
            Box::pin(async move { stream.collect().await.1 })
        }
    });
    let prepared_files =
        prepare_file_arguments(&args.file_args, &cwd, settings.get_image_auto_resize())?;
    let mut prompts: Vec<(String, Vec<ContentBlock>)> = Vec::new();
    let stdin_content = args.stdin_content.as_deref().unwrap_or_default();
    if let Some((file_text, images)) = prepared_files {
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_text = format!("{stdin_content}{file_text}{first_message}");
        if !initial_text.is_empty() || !images.is_empty() {
            prompts.push((initial_text, images));
        }
        prompts.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .map(|text| (text.clone(), Vec::new())),
        );
    } else {
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_text = format!("{stdin_content}{first_message}");
        if !initial_text.is_empty() {
            prompts.push((initial_text, Vec::new()));
        }
        prompts.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .map(|text| (text.clone(), Vec::new())),
        );
    }
    for (text, images) in prompts {
        let expanded =
            crate::core::prompt_templates::expand_prompt_template(&text, &prompt_templates);
        let mut blocks = vec![ContentBlock::text(expanded)];
        blocks.extend(images);
        let prompt = pi_agent::types::AgentMessage::Core(Message::User(UserContent::blocks(
            blocks,
            pi_ai::types::now_ms(),
        )));
        let turn_messages = harness
            .run_prompt(vec![prompt])
            .await
            .map_err(|error| format!("run harness prompt: {error}"))?;
        let mut history_entries = harness
            .transcript()
            .await
            .map_err(|error| format!("read harness transcript: {error}"))?;
        all_messages.extend(turn_messages);

        let mut agent_messages = harness
            .agent_messages()
            .await
            .map_err(|error| format!("read harness messages: {error}"))?;
        if let Some(compaction) = maybe_auto_compact(
            &mut agent_messages,
            &mut history_entries,
            &model,
            &settings,
            &summarizer,
        )
        .await
        {
            harness
                .set_agent_messages(agent_messages)
                .await
                .map_err(|error| format!("set compacted harness messages: {error}"))?;
            harness
                .append_entry(compaction)
                .await
                .map_err(|error| format!("append harness compaction: {error}"))?;
        }
    }

    // The last assistant message drives output (upstream print-mode.ts reads
    // `state.messages[state.messages.length - 1]`).
    let last_assistant = all_messages.iter().rev().find_map(|m| match m {
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => Some(a),
        _ => None,
    });

    // Terminal error/abort: print `errorMessage` or `Request {stopReason}` to
    // stderr and exit nonzero (upstream sets exitCode = 1).
    if let Some(a) = last_assistant {
        if matches!(
            a.stop_reason(),
            Some(pi_ai::types::StopReason::Error) | Some(pi_ai::types::StopReason::Aborted)
        ) {
            let msg = a
                .error_message()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Request {}", a.stop_reason().unwrap().as_str()));
            return Err(crate::core::auth_guidance::format_provider_auth_failure(
                a.provider().unwrap_or(&provider),
                selected_provider_uses_oauth,
                &msg,
            ));
        }
    }

    // Text-mode output: each text content block printed with a trailing newline
    // (upstream `writeRawStdout(`${content.text}\n`)`), so blocks are joined
    // with `\n` rather than concatenated.
    let final_text: String = last_assistant
        .map(|a| {
            a.content()
                .iter()
                .filter_map(|b| match b {
                    pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    Ok(RunOutcome {
        final_text,
        session_path: durable_session_path,
    })
}

/// Select or create the session used by the one-shot CLI path.
///
/// The TypeScript CLI resolves these selectors before `runPrintMode` starts:
/// `--continue` chooses the newest session for the current directory,
/// `--resume` resolves a session target (or the newest available target in
/// non-interactive mode), and `--fork` opens a child created from the selected
/// source. Keeping that decision here means the harness can append directly to
/// the selected durable file instead of replaying a fresh in-memory transcript
/// into a second session at shutdown.
#[allow(dead_code)]
pub(crate) async fn prepare_run_session(
    args: &Args,
    cwd: &str,
) -> Result<(Session<StdFileSystem>, Option<String>), String> {
    prepare_run_session_with_lifecycle(args, cwd, None).await
}

/// Select or create the durable v3 session used by JSON mode. The wire and
/// file formats are explicit so native pi-agent session callers retain v4.
pub(crate) async fn prepare_run_session_v3(
    args: &Args,
    cwd: &str,
) -> Result<(Session<StdFileSystem>, Option<String>), String> {
    prepare_run_session_with_settings(args, cwd, None, None, true).await
}

/// Resolve the durable session root with the same precedence as the
/// TypeScript runtime: CLI `--session-dir`, then the dedicated environment
/// variable, then the loaded settings value, then the default agent root.
/// Keeping this in one helper prevents print and interactive modes from
/// silently ignoring a configured `sessionDir`.
pub(crate) fn resolve_session_root(args: &Args, settings: Option<&SettingsManager>) -> String {
    args.session_dir
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(config::expand_tilde_path)
        .or_else(|| {
            config::env(config::ENV_SESSION_DIR)
                .filter(|path| !path.is_empty())
                .map(|path| config::expand_tilde_path(&path))
        })
        .or_else(|| settings.and_then(SettingsManager::get_session_dir))
        .unwrap_or_else(|| config::get_session_dir().to_string_lossy().into_owned())
}

/// Session selectors used by the initial CLI run are normally resolved before
/// the agent loop exists. Keep that ordering, but still give the already
/// loaded extension runtime the same veto point used by runtime session
/// replacement. `--fork` has no entry selector, so its source session id is
/// carried in the upstream `entryId` field as the only stable identifier
/// available at this boundary.
#[allow(dead_code)]
async fn prepare_run_session_with_lifecycle(
    args: &Args,
    cwd: &str,
    extension_runner: Option<&crate::core::extensions::ExtensionRunner>,
) -> Result<(Session<StdFileSystem>, Option<String>), String> {
    prepare_run_session_with_settings(args, cwd, extension_runner, None, false).await
}

async fn prepare_run_session_with_settings(
    args: &Args,
    cwd: &str,
    extension_runner: Option<&crate::core::extensions::ExtensionRunner>,
    settings: Option<&SettingsManager>,
    v3: bool,
) -> Result<(Session<StdFileSystem>, Option<String>), String> {
    let selects_existing =
        args.continue_session || args.resume || args.session.is_some() || args.fork.is_some();

    if args.no_session {
        if selects_existing {
            return Err(
                "--continue, --resume, --session, and --fork require session persistence"
                    .to_string(),
            );
        }
        // Upstream keeps an explicitly supplied --session-id on ephemeral
        // sessions as well.  The session is still non-persistent, but its
        // identity must remain observable to the runtime and its hooks.
        let session_id = args
            .session_id
            .as_deref()
            .unwrap_or("print-run")
            .to_string();
        let mut metadata = in_memory_metadata(session_id, None);
        // `SessionManager.inMemory(cwd)` still exposes a v3 session header to
        // JSON mode.  Keep the in-memory session non-persistent, but retain
        // the real launch directory in that observable header.
        metadata.cwd = cwd.to_string();
        let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(metadata)));
        return Ok((Session::from_in_memory(storage), None));
    }

    let session_root = resolve_session_root(args, settings);
    let session_root_path = PathBuf::from(&session_root);
    std::fs::create_dir_all(&session_root_path)
        .map_err(|error| format!("create session dir {session_root}: {error}"))?;

    // Migrate all files visible to the normal repository inventory before
    // resolving a selector. An explicit path outside the configured root is
    // migrated below as well.
    crate::core::session_migration::migrate_legacy_sessions_in_root(&session_root_path)
        .map_err(|error| format!("migrate legacy sessions: {error}"))?;

    let fs = StdFileSystem::new(cwd);
    let mut repo = JsonlSessionRepo::new(fs, &session_root);
    let source_selector = args.fork.as_deref().or(args.session.as_deref());
    if args.fork.is_some() {
        if let Some(session_id) = args.session_id.as_deref() {
            if find_local_session_by_id(&repo, cwd, session_id)
                .await?
                .is_some()
            {
                return Err(format!("Session already exists with id '{session_id}'"));
            }
        }
    }
    let source = if let Some(selector) = source_selector {
        let path = resolve_session_selector_path(selector, cwd);
        if path.is_file() {
            crate::core::session_migration::migrate_legacy_session_file(&path)
                .map_err(|error| format!("migrate selected session: {error}"))?;
        }
        Some(resolve_session_metadata(&repo, selector, cwd).await?)
    } else if args.continue_session || args.resume {
        let mut sessions = repo
            .list(Some(cwd))
            .await
            .map_err(|error| format!("list sessions: {error}"))?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_at));
        Some(sessions.into_iter().next().ok_or_else(|| {
            if args.resume {
                "no sessions found to resume in this directory".to_string()
            } else {
                "no previous session found to continue in this directory".to_string()
            }
        })?)
    } else {
        None
    };

    if let (Some(runner), Some(source)) = (extension_runner, source.as_ref()) {
        if args.fork.is_some() {
            let cancelled =
                runner
                    .emit_session_before_fork(&source.id, "at")
                    .map_err(|errors| {
                        let details = errors
                            .into_iter()
                            .map(|error| {
                                format!(
                                    "{} [{}]: {}",
                                    error.extension_path, error.event, error.error
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        format!("extension session_before_fork failed: {details}")
                    })?;
            if cancelled {
                return Err("initial session fork cancelled by extension".to_string());
            }
        } else if args.session.is_some() || args.continue_session || args.resume {
            let cancelled = runner
                .emit_session_before_switch("resume", Some(&source.path))
                .map_err(|errors| {
                    let details = errors
                        .into_iter()
                        .map(|error| {
                            format!(
                                "{} [{}]: {}",
                                error.extension_path, error.event, error.error
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    format!("extension session_before_switch failed: {details}")
                })?;
            if cancelled {
                return Err("initial session resume cancelled by extension".to_string());
            }
        }
    }

    let mut session = if let Some(source) = source {
        if args.fork.is_some() {
            let fork_options = CreateOptions {
                id: args
                    .session_id
                    .clone()
                    .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
                cwd: cwd.to_string(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            };
            if v3 {
                repo.fork_v3(&source, fork_options).await
            } else {
                repo.fork(&source, fork_options).await
            }
            .map_err(|error| format!("fork session {}: {error}", source.id))?
        } else {
            repo.open(&source)
                .await
                .map_err(|error| format!("open session {}: {error}", source.id))?
        }
    } else if let Some(session_id) = args.session_id.as_deref() {
        if let Some(existing) = find_local_session_by_id(&repo, cwd, session_id).await? {
            repo.open(&existing)
                .await
                .map_err(|error| format!("open session {}: {error}", existing.id))?
        } else {
            eprintln!(
                "Warning: No project session found with id '{session_id}'; creating a new session with that id."
            );
            let create_options = CreateOptions {
                id: Some(session_id.to_string()),
                cwd: cwd.to_string(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            };
            if v3 {
                repo.create_v3(create_options).await
            } else {
                repo.create(create_options).await
            }
            .map_err(|error| format!("create session: {error}"))?
        }
    } else {
        let create_options = CreateOptions {
            id: args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
            cwd: cwd.to_string(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Tree,
        };
        if v3 {
            repo.create_v3(create_options).await
        } else {
            repo.create(create_options).await
        }
        .map_err(|error| format!("create session: {error}"))?
    };

    // The upstream CLI validates the selected session's stored working
    // directory before entering any non-interactive mode.  Keep the new
    // session's cwd as the fallback, but do not silently run a resumed
    // session from a different directory when its original project vanished.
    // Interactive startup performs the same check with its trust-aware
    // selector, so this guard is intentionally scoped to this shared
    // non-interactive preparation path.
    let session_metadata = session.get_metadata().await;
    crate::core::session_cwd::assert_session_cwd_exists(
        Some(&session_metadata.path),
        &session_metadata.cwd,
        cwd,
    )
    .map_err(|error| error.to_string())?;

    if let Some(name) = &args.name {
        let normalized_name = normalize_session_name_value(name);
        if normalized_name.is_empty() {
            return Err("--name requires a non-empty value".to_string());
        }
        session
            .set_name(Some(&normalized_name))
            .await
            .map_err(|error| format!("set session name: {error}"))?;
    }
    let path = session.get_metadata().await.path;
    Ok((session, Some(path)))
}

/// Find an exact session id in the current project, matching the pinned CLI's
/// `findLocalSessionByExactId` check. Prefixes and sessions from other
/// projects are intentionally excluded from this caller-level operation.
pub(crate) async fn find_local_session_by_id(
    repo: &JsonlSessionRepo<StdFileSystem>,
    cwd: &str,
    session_id: &str,
) -> Result<Option<SessionMetadata>, String> {
    repo.list(Some(cwd))
        .await
        .map(|sessions| {
            sessions
                .into_iter()
                .find(|session| session.id == session_id)
        })
        .map_err(|error| format!("list sessions: {error}"))
}

/// Resolve a CLI session selector by path, exact id, or an unambiguous id
/// prefix. The search order matches the pinned CLI: current-project exact id,
/// current-project prefix, global exact id, then global prefix. Paths are
/// resolved relative to the caller's cwd and may refer to a file outside the
/// configured session root.
pub(crate) async fn resolve_session_metadata(
    repo: &JsonlSessionRepo<StdFileSystem>,
    selector: &str,
    cwd: &str,
) -> Result<SessionMetadata, String> {
    let requested_path = resolve_session_selector_path(selector, cwd);
    let path_like = selector.ends_with(".jsonl")
        || selector.contains(std::path::MAIN_SEPARATOR)
        || selector.contains('/')
        || selector.contains('\\');

    if path_like {
        if requested_path.is_file() {
            return metadata_from_session_path(&requested_path);
        }
        return Err(format!("session not found: {selector}"));
    }

    let local_sessions = repo
        .list(Some(cwd))
        .await
        .map_err(|error| format!("list sessions: {error}"))?;
    if let Some(metadata) = find_exact_or_prefix_session(local_sessions, selector) {
        return Ok(metadata);
    }

    let global_sessions = repo
        .list(None)
        .await
        .map_err(|error| format!("list sessions: {error}"))?;
    find_exact_or_prefix_session(global_sessions, selector)
        .ok_or_else(|| format!("session not found: {selector}"))
}

fn find_exact_or_prefix_session(
    sessions: Vec<SessionMetadata>,
    selector: &str,
) -> Option<SessionMetadata> {
    sessions
        .iter()
        .find(|metadata| metadata.id == selector)
        .cloned()
        .or_else(|| {
            sessions
                .into_iter()
                .find(|metadata| metadata.id.starts_with(selector))
        })
}

/// Resolve a path-like session argument with the same cwd-relative semantics
/// as upstream `resolvePath`. Keeping this separate from metadata lookup also
/// ensures legacy migration and the eventual open use the same physical path.
pub(crate) fn resolve_session_selector_path(selector: &str, cwd: &str) -> PathBuf {
    // `resolvePath` in the pinned CLI also accepts file URLs.  Convert those
    // before lexical normalization so an explicit URL and its filesystem
    // spelling select the same session file.
    let expanded = config::expand_tilde_path(selector);
    let expanded = expanded
        .strip_prefix("file://")
        .and_then(|url_path| {
            url::Url::parse(&format!("file://{url_path}"))
                .ok()
                .and_then(|url| url.to_file_path().ok())
        })
        .unwrap_or_else(|| PathBuf::from(expanded));
    if expanded.is_absolute() {
        normalize_session_selector_path(expanded)
    } else {
        normalize_session_selector_path(Path::new(cwd).join(expanded))
    }
}

/// Lexically normalize a session selector without requiring the target to
/// exist.  This matches Node's `resolvePath` behavior for explicit session
/// paths while preserving a root/prefix component when `..` reaches it.
fn normalize_session_selector_path(path: PathBuf) -> PathBuf {
    use std::path::Component;

    let absolute = path.is_absolute();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(component.as_os_str())),
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if can_pop {
                    normalized.pop();
                } else if !absolute {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

/// Normalize a caller-provided session display name before it reaches the
/// session storage layer. The upstream session manager collapses every
/// contiguous CR/LF run to one space and trims the result; doing that here
/// keeps all coding-agent modes safe even though the lower session crate is
/// intentionally a generic storage API.
pub fn normalize_session_name_value(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut in_line_break = false;
    for character in name.chars() {
        if matches!(character, '\r' | '\n') {
            if !in_line_break {
                normalized.push(' ');
                in_line_break = true;
            }
        } else {
            normalized.push(character);
            in_line_break = false;
        }
    }
    normalized.trim().to_string()
}

/// Read the v4 header for an explicit session file that is not in the
/// configured repository root. The repository only needs this metadata to
/// validate/open the file; entries remain decoded by `JsonlSessionRepo::open`.
pub(crate) fn metadata_from_session_path(path: &Path) -> Result<SessionMetadata, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("read session {}: {error}", path.display()))?;
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| format!("session {} is empty", path.display()))?;
    let header: serde_json::Value = serde_json::from_str(first_line)
        .map_err(|error| format!("parse session header {}: {error}", path.display()))?;
    let is_v4 = header.get("kind").and_then(serde_json::Value::as_str) == Some("header");
    let is_v3 = header.get("type").and_then(serde_json::Value::as_str) == Some("session");
    if !is_v4 && !is_v3 {
        return Err(format!(
            "session {} is not a supported JSONL file",
            path.display()
        ));
    }
    if is_v3 {
        let parsed = pi_agent::session::jsonl::parse_v3_header(first_line)
            .map_err(|error| format!("parse session header {}: {error}", path.display()))?;
        let modified_at = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        return Ok(SessionMetadata {
            id: parsed.id,
            created_at: parsed.created_at,
            cwd: parsed.cwd,
            path: path.to_string_lossy().into_owned(),
            modified_at,
            source_format: 3,
            parent_session_id: parsed.parent_session_id,
            legacy_parent_session_path: parsed.legacy_parent_session_path,
            metadata: parsed.metadata,
        });
    }
    let id = header
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("session {} header is missing id", path.display()))?;
    let modified_at = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Ok(SessionMetadata {
        id: id.to_string(),
        created_at: header
            .get("createdAt")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cwd: header
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        path: path.to_string_lossy().into_owned(),
        modified_at,
        source_format: header
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(4),
        parent_session_id: header
            .get("parentSessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        legacy_parent_session_path: header
            .get("legacyParentSessionPath")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        metadata: header.get("metadata").cloned(),
    })
}

/// Apply one threshold compaction to the print-mode context and return the
/// provisioned JSONL entry to append after the turn's messages.
async fn maybe_auto_compact(
    messages: &mut Vec<AgentMessage>,
    history_entries: &mut Vec<Entry>,
    model: &pi_ai::model::Model,
    settings: &SettingsManager,
    summarizer: &SimpleModels,
) -> Option<EntryNoStats> {
    let (enabled, reserve_tokens, keep_recent_tokens) = settings.get_compaction_settings();
    let compaction_settings = CompactionSettings {
        enabled,
        reserve_tokens,
        keep_recent_tokens,
    };
    let estimate = estimate_context_tokens(messages);
    if !should_compact(estimate.tokens, model.context_window, &compaction_settings) {
        return None;
    }

    let preparation = match prepare_compaction(history_entries, &compaction_settings) {
        Ok(preparation) => preparation,
        Err(error) => {
            tracing::warn!(%error, "automatic print-mode compaction preparation failed");
            return None;
        }
    };
    let preparation = preparation?;
    let result = match compact(
        &preparation,
        summarizer,
        model,
        None,
        None,
        Some("off"),
        None,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "automatic print-mode compaction failed");
            return None;
        }
    };

    let id = pi_agent::session::new_id();
    let seq = history_entries.last().map_or(1, |entry| entry.seq() + 1);
    let parent_id = history_entries.last().map(|entry| entry.id().to_string());
    let timestamp = pi_ai::types::now_ms();
    let details = result.details.as_ref().map(|details| {
        serde_json::json!({
            "readFiles": details.read_files,
            "modifiedFiles": details.modified_files,
        })
    });
    let retained_tail = result.retained_tail.clone();
    let summary = result.summary.clone();
    let tokens_before = result.tokens_before;
    let usage = result.usage.clone();
    history_entries.push(Entry::Compaction {
        id: id.clone(),
        seq,
        parent_id,
        timestamp,
        summary: summary.clone(),
        retained_tail: retained_tail.clone(),
        tokens_before,
        details: details.clone(),
        usage: usage.clone(),
    });
    *messages =
        build_session_context(history_entries, &SessionContextBuildOptions::default()).messages;

    Some(EntryNoStats::Compaction {
        id,
        summary,
        retained_tail,
        tokens_before,
        details,
        usage,
    })
}

/// Process `@file` arguments using the coding-agent image pipeline. Text
/// files become tagged prompt text; image files become model-facing image
/// blocks plus the same `<file>` reference used by upstream.
pub(crate) fn prepare_file_arguments(
    file_args: &[String],
    cwd: &str,
    auto_resize_images: bool,
) -> Result<Option<(String, Vec<ContentBlock>)>, String> {
    if file_args.is_empty() {
        return Ok(None);
    }

    let mut text = String::new();
    let mut images = Vec::new();
    for file_arg in file_args {
        let absolute = pi_agent::tools::path_utils::resolve_read_tool_path_existing(cwd, file_arg);
        let metadata = std::fs::metadata(&absolute)
            .map_err(|_| format!("Error: File not found: {absolute}"))?;
        if metadata.len() == 0 {
            continue;
        }
        let bytes = std::fs::read(&absolute)
            .map_err(|error| format!("Error: Could not read file {absolute}: {error}"))?;
        if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
            match process_image(
                &bytes,
                mime_type,
                ProcessImageOptions {
                    auto_resize_images,
                    ..Default::default()
                },
            ) {
                Ok(processed) => {
                    images.push(ContentBlock::image(processed.data, processed.mime_type));
                    if processed.hints.is_empty() {
                        text.push_str(&format!("<file name=\"{absolute}\"></file>\n"));
                    } else {
                        text.push_str(&format!(
                            "<file name=\"{absolute}\">{}</file>\n",
                            processed.hints.join("\n")
                        ));
                    }
                }
                Err(message) => {
                    text.push_str(&format!("<file name=\"{absolute}\">{message}</file>\n"));
                }
            }
        } else {
            let content = String::from_utf8(bytes)
                .map_err(|error| format!("Error: Could not read file {absolute}: {error}"))?;
            let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
            text.push_str(&format!("<file name=\"{absolute}\">\n{content}\n</file>\n"));
        }
    }
    Ok(Some((text, images)))
}

#[derive(Clone, Copy)]
struct BuiltinToolPromptContribution {
    name: &'static str,
    snippet: &'static str,
    guidelines: &'static [&'static str],
}

// These are the prompt contributions exported by the pinned upstream
// built-in tool definitions.  Keep them here, next to prompt assembly, so the
// model-facing description cannot drift from the active tool registry.
static BUILTIN_TOOL_PROMPT_CONTRIBUTIONS: &[BuiltinToolPromptContribution] = &[
    BuiltinToolPromptContribution {
        name: "read",
        snippet: "Read file contents",
        guidelines: &["Use read to examine files instead of cat or sed."],
    },
    BuiltinToolPromptContribution {
        name: "bash",
        snippet: "Execute bash commands (ls, grep, find, etc.)",
        guidelines: &["You can inspect PI_* environment variables for current model and session details."],
    },
    BuiltinToolPromptContribution {
        name: "edit",
        snippet: "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
        guidelines: &[
            "Use edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ],
    },
    BuiltinToolPromptContribution {
        name: "write",
        snippet: "Create or overwrite files",
        guidelines: &["Use write only for new files or complete rewrites."],
    },
    BuiltinToolPromptContribution {
        name: "grep",
        snippet: "Search file contents for patterns (respects .gitignore)",
        guidelines: &[],
    },
    BuiltinToolPromptContribution {
        name: "find",
        snippet: "Find files by glob pattern (respects .gitignore)",
        guidelines: &[],
    },
    BuiltinToolPromptContribution {
        name: "ls",
        snippet: "List directory contents",
        guidelines: &[],
    },
];

const BUILTIN_TOOL_NAMES: [&str; 7] = ["bash", "read", "write", "edit", "ls", "find", "grep"];
const DEFAULT_ACTIVE_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

fn is_builtin_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name)
}

/// Explicit `--tools` is an allowlist and therefore overrides either broad
/// suppression flag, matching upstream `options.tools ?? options.noTools`.
pub(crate) fn should_register_builtin_tools(args: &Args) -> bool {
    args.tools.is_some() || (!args.no_tools && !args.no_builtin_tools)
}

pub(crate) fn should_register_extension_tools(args: &Args) -> bool {
    args.tools.is_some() || !args.no_tools
}

fn builtin_tool_prompt_contribution(name: &str) -> Option<&'static BuiltinToolPromptContribution> {
    BUILTIN_TOOL_PROMPT_CONTRIBUTIONS
        .iter()
        .find(|contribution| contribution.name == name)
}

/// Resolve the upstream initial active-tool policy while retaining the order
/// supplied by the caller.  `available_tool_names` contains the actual
/// registry, so extension tools are included by default and unknown explicit
/// names are discarded by the caller after policy resolution.
fn active_tool_names_for_policy(
    args: &Args,
    settings: &SettingsManager,
    available_tool_names: &[String],
) -> Vec<String> {
    let mut active = if let Some(explicit) = &args.tools {
        explicit.clone()
    } else if args.no_tools {
        Vec::new()
    } else if args.no_builtin_tools {
        available_tool_names
            .iter()
            .filter(|name| !is_builtin_tool_name(name))
            .cloned()
            .collect()
    } else {
        settings.get_default_tools().unwrap_or_else(|| {
            DEFAULT_ACTIVE_TOOL_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect()
        })
    };

    // `includeAllExtensionTools: true` is the upstream default when no
    // explicit allowlist or suppression mode was supplied.
    if args.tools.is_none() && !args.no_tools && !args.no_builtin_tools {
        active.extend(
            available_tool_names
                .iter()
                .filter(|name| !is_builtin_tool_name(name))
                .cloned(),
        );
    }

    if let Some(excluded) = &args.exclude_tools {
        active.retain(|name| !excluded.iter().any(|excluded| excluded == name));
    }

    let mut unique = Vec::with_capacity(active.len());
    for name in active {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    unique
}

/// Assemble a prompt from the actual filtered tool vector used by print mode.
/// Extension prompt snippets/guidelines come from the same loaded definitions
/// that created those tools, while built-in contributions are the pinned
/// upstream constants above.
pub(crate) fn assemble_run_system_prompt_with_active_tools(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
    extension_resources: &crate::core::extensions::ResourceDiscovery,
    active_tools: &[pi_agent::tools::AgentTool],
    extension_tool_definitions: &[crate::core::extensions::RegisteredTool],
) -> String {
    let mut active_tool_names = Vec::with_capacity(active_tools.len());
    for tool in active_tools {
        if !active_tool_names.contains(&tool.tool.name) {
            active_tool_names.push(tool.tool.name.clone());
        }
    }
    assemble_system_prompt_from_active_tool_names(
        args,
        cwd,
        agent_dir,
        settings,
        extension_resources,
        &active_tool_names,
        extension_tool_definitions,
    )
}

/// Assemble the prompt form for callers that only have resource discovery.
/// Callers with a loaded tool registry should use the active-tool overload
/// above so extension snippets and guidelines follow the actual tool set.
pub(crate) fn assemble_run_system_prompt(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
    extension_resources: &crate::core::extensions::ResourceDiscovery,
) -> String {
    let available_tool_names = BUILTIN_TOOL_NAMES
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    let active_tool_names = active_tool_names_for_policy(args, settings, &available_tool_names);
    assemble_system_prompt_from_active_tool_names(
        args,
        cwd,
        agent_dir,
        settings,
        extension_resources,
        &active_tool_names,
        &[],
    )
}

fn assemble_system_prompt_from_active_tool_names(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
    extension_resources: &crate::core::extensions::ResourceDiscovery,
    active_tool_names: &[String],
    extension_tool_definitions: &[crate::core::extensions::RegisteredTool],
) -> String {
    // `DefaultResourceLoader` discovers these two files after CLI sources have
    // been applied. Keep the same precedence in the shared Rust prompt path:
    // an explicit source wins, then a trusted project file, then the global
    // agent file. Empty CLI values retain the upstream default/no-append
    // behavior rather than accidentally shadowing a discovered file.
    let system_prompt_source = args.system_prompt.clone().or_else(|| {
        discover_prompt_source(cwd, agent_dir, settings, "SYSTEM.md")
            .map(|path| path.to_string_lossy().into_owned())
    });
    let custom = system_prompt_source
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
        .map(|prompt| resolve_prompt_input(prompt, "system prompt"))
        .filter(|prompt| !prompt.is_empty());
    let is_custom = custom.is_some();
    let mut prompt = custom
        .unwrap_or_else(|| default_system_prompt(active_tool_names, extension_tool_definitions));

    let append_sources = if args.append_system_prompt.is_empty() {
        discover_prompt_source(cwd, agent_dir, settings, "APPEND_SYSTEM.md")
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    } else {
        args.append_system_prompt.clone()
    };
    let append_sections = append_sources
        .iter()
        .map(|append| resolve_prompt_input(append, "append system prompt"))
        .filter(|append| !append.is_empty())
        .collect::<Vec<_>>();
    if !append_sections.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&append_sections.join("\n\n"));
    }

    // The pinned upstream order is append prompt, project context, skills,
    // then cwd.  The resource formatters already provide their own leading
    // separators, so append them directly to preserve those exact boundaries.
    if !args.no_context_files {
        let context_files = crate::core::context_files::load_project_context_files(
            cwd,
            &agent_dir.display().to_string(),
        );
        let context = crate::core::context_files::format_project_context(&context_files);
        if !context.is_empty() {
            prompt.push_str(&context);
        }
    }

    let skills_block = build_skills_block(args, cwd, agent_dir, settings, extension_resources);
    let has_read = active_tool_names.iter().any(|name| name == "read");
    if has_read && !skills_block.is_empty() {
        prompt.push_str(&skills_block);
    }

    prompt.push_str("\nCurrent working directory: ");
    prompt.push_str(&cwd.replace('\\', "/"));
    if is_custom {
        prompt.push('\n');
    }
    prompt
}

fn discover_prompt_source(
    cwd: &str,
    agent_dir: &Path,
    settings: &SettingsManager,
    filename: &str,
) -> Option<PathBuf> {
    let project_path = Path::new(cwd)
        .join(crate::config::CONFIG_DIR_NAME)
        .join(filename);
    if settings.is_project_trusted() && project_path.exists() {
        return Some(project_path);
    }

    let global_path = agent_dir.join(filename);
    global_path.exists().then_some(global_path)
}

fn default_system_prompt(
    active_tool_names: &[String],
    extension_tool_definitions: &[crate::core::extensions::RegisteredTool],
) -> String {
    let mut visible_tools = Vec::new();
    let mut guidelines = Vec::new();
    let mut add_guideline = |guideline: &str| {
        let normalized = guideline.trim();
        if !normalized.is_empty() && !guidelines.iter().any(|entry| entry == normalized) {
            guidelines.push(normalized.to_string());
        }
    };

    let has_bash = active_tool_names.iter().any(|name| name == "bash");
    let has_grep = active_tool_names.iter().any(|name| name == "grep");
    let has_find = active_tool_names.iter().any(|name| name == "find");
    let has_ls = active_tool_names.iter().any(|name| name == "ls");
    if has_bash && !has_grep && !has_find && !has_ls {
        add_guideline("Use bash for file operations like ls, rg, find");
    }

    for name in active_tool_names {
        let builtin = builtin_tool_prompt_contribution(name);
        let extension = extension_tool_definitions
            .iter()
            .find(|definition| definition.name.as_str() == name.as_str());
        if let Some(snippet) = builtin
            .map(|contribution| contribution.snippet.to_string())
            .or_else(|| extension.and_then(|definition| definition.prompt_snippet.clone()))
            .and_then(|snippet| normalize_prompt_snippet(&snippet))
        {
            visible_tools.push(format!("- {name}: {snippet}"));
        }

        if let Some(contribution) = builtin {
            for guideline in contribution.guidelines {
                add_guideline(guideline);
            }
        } else if let Some(definition) = extension {
            if let Some(extension_guidelines) = &definition.prompt_guidelines {
                for guideline in extension_guidelines {
                    add_guideline(guideline);
                }
            }
        }
    }

    add_guideline("Be concise in your responses");
    add_guideline("Show file paths clearly when working with files");

    let docs = crate::core::auth_guidance::get_docs_path();
    let package_dir = docs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let tools = if visible_tools.is_empty() {
        "(none)".to_string()
    } else {
        visible_tools.join("\n")
    };
    let guidelines = guidelines
        .into_iter()
        .map(|guideline| format!("- {guideline}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n{guidelines}\n\nPi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: {}\n- Additional docs: {}\n- Examples: {} (extensions, custom tools, SDK)\n- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md), environment variables (docs/environment-variables.md)\n- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
        package_dir.join("README.md").display(),
        docs.display(),
        package_dir.join("examples").display(),
    )
}

fn normalize_prompt_snippet(value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

/// Apply the same initial active-tool policy as the upstream AgentSession.
/// The tool registry may contain every built-in and native extension tool, but
/// only the selected names are sent to the model or allowed to execute.
pub(crate) fn select_active_tools(
    args: &Args,
    settings: &SettingsManager,
    tools: Vec<pi_agent::tools::AgentTool>,
) -> Vec<pi_agent::tools::AgentTool> {
    let available_tool_names = tools
        .iter()
        .map(|tool| tool.tool.name.clone())
        .collect::<Vec<_>>();
    let active_tool_names = active_tool_names_for_policy(args, settings, &available_tool_names);
    active_tool_names
        .into_iter()
        .filter_map(|name| tools.iter().find(|tool| tool.tool.name == name).cloned())
        .collect()
}

/// If `input` is an existing file path, read its contents (stripping a BOM);
/// otherwise return the input verbatim (upstream `resolvePromptInput`).
fn resolve_prompt_input(input: &str, description: &str) -> String {
    let expanded = config::expand_tilde_path(input);
    let path = std::path::Path::new(&expanded);
    if path.is_file() {
        match std::fs::read_to_string(path) {
            Ok(content) => return content.trim_start_matches('\u{feff}').to_string(),
            Err(e) => {
                tracing::warn!("could not read {description} file {input}: {e}");
                return input.to_string();
            }
        }
    }
    input.to_string()
}

fn resource_contains_path(
    resource: &crate::core::extensions::DiscoveredResource,
    loaded_path: &str,
    cwd: &str,
) -> bool {
    let resource_path = PathBuf::from(resource.resolved_path(cwd));
    let loaded_path = crate::core::extensions::loader::resolve_relative_path(loaded_path, cwd);
    loaded_path == resource_path || loaded_path.starts_with(&resource_path)
}

fn apply_extension_skill_source_info(
    skills: &mut [crate::core::skills::Skill],
    resources: &[crate::core::extensions::DiscoveredResource],
    cwd: &str,
) {
    for skill in skills {
        if let Some(resource) = resources
            .iter()
            .find(|resource| resource_contains_path(resource, &skill.file_path, cwd))
        {
            let mut source_info = resource.source_info.clone();
            source_info.path = skill.file_path.clone();
            skill.source_info = source_info;
        }
    }
}

fn apply_extension_prompt_source_info(
    templates: &mut [crate::core::prompt_templates::PromptTemplate],
    resources: &[crate::core::extensions::DiscoveredResource],
    cwd: &str,
) {
    for template in templates {
        if let Some(resource) = resources
            .iter()
            .find(|resource| resource_contains_path(resource, &template.file_path, cwd))
        {
            let mut source_info = resource.source_info.clone();
            source_info.path = template.file_path.clone();
            template.source_info = source_info;
        }
    }
}

/// Load skills (user + project + `--skill`) and render the `<available_skills>`
/// system-prompt block, marking `-ns` disabled. Surfaces load diagnostics as
/// warnings.
pub(crate) fn build_skills_block(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
    extension_resources: &crate::core::extensions::ResourceDiscovery,
) -> String {
    // `--no-skills` disables automatic settings/user/project discovery, but
    // pinned upstream still loads explicit CLI and extension paths.  Keep the
    // two sources separate so a disabled discovery flag cannot erase an
    // explicitly requested skill.
    let mut skill_paths: Vec<String> = if args.no_skills {
        Vec::new()
    } else {
        settings.get_skill_paths()
    };
    skill_paths.extend(args.skills.iter().cloned());
    skill_paths.extend(extension_resources.resolved_skill_paths(cwd));
    let options = crate::core::skills::LoadSkillsOptions {
        cwd: cwd.to_string(),
        agent_dir: agent_dir.display().to_string(),
        skill_paths,
    };
    let (mut skills, diagnostics) = if args.no_skills {
        crate::core::skills::load_skills_without_defaults(options)
    } else {
        crate::core::skills::load_skills(options)
    };
    apply_extension_skill_source_info(&mut skills, &extension_resources.skill_resources, cwd);
    for diagnostic in &diagnostics {
        tracing::warn!(
            path = ?diagnostic.path,
            message = %diagnostic.message,
            "skill load diagnostic"
        );
    }
    crate::core::skills::format_skills_for_prompt(&skills)
}

/// Load prompt templates (user + project + `--prompt-template`) for run-path
/// expansion, marking `-np` / `-npt` disabled.
pub(crate) fn load_prompt_templates_for_run(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    extension_resources: &crate::core::extensions::ResourceDiscovery,
) -> Vec<crate::core::prompt_templates::PromptTemplate> {
    load_prompt_templates_for_run_with_paths(args, cwd, agent_dir, &[], extension_resources)
}

/// Run-path prompt loading with persisted resource paths included. The
/// resource loader keeps configured paths separate from CLI paths: `--np`
/// suppresses configured/discovered templates but explicit `--prompt-template`
/// paths remain active. Keep the legacy helper above for mode callers that
/// already assemble their own settings paths.
pub(crate) fn load_prompt_templates_for_run_with_settings(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    settings: &SettingsManager,
    extension_resources: &crate::core::extensions::ResourceDiscovery,
) -> Vec<crate::core::prompt_templates::PromptTemplate> {
    let configured_paths = if args.no_prompt_templates {
        Vec::new()
    } else {
        configured_prompt_paths(settings, cwd, agent_dir)
    };
    load_prompt_templates_for_run_with_paths(
        args,
        cwd,
        agent_dir,
        &configured_paths,
        extension_resources,
    )
}

fn configured_prompt_paths(
    settings: &SettingsManager,
    cwd: &str,
    agent_dir: &std::path::Path,
) -> Vec<String> {
    let project_base = Path::new(cwd).join(crate::config::CONFIG_DIR_NAME);
    let global_base = agent_dir.to_path_buf();
    let mut paths = Vec::new();
    for (scope, base) in [
        (settings.get_project_settings(), project_base),
        (settings.get_global_settings(), global_base),
    ] {
        let Some(values) = scope.get("prompts").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let base = base.to_string_lossy();
        paths.extend(
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|raw| {
                    crate::core::extensions::loader::resolve_relative_path(raw, &base)
                        .to_string_lossy()
                        .into_owned()
                }),
        );
    }
    paths
}

fn load_prompt_templates_for_run_with_paths(
    args: &Args,
    cwd: &str,
    agent_dir: &std::path::Path,
    configured_paths: &[String],
    extension_resources: &crate::core::extensions::ResourceDiscovery,
) -> Vec<crate::core::prompt_templates::PromptTemplate> {
    let mut prompt_paths = configured_paths.to_vec();
    prompt_paths.extend(args.prompt_templates.iter().cloned());
    prompt_paths.extend(extension_resources.resolved_prompt_paths(cwd));
    let (mut templates, diagnostics) = crate::core::prompt_templates::load_prompt_templates(
        cwd,
        &agent_dir.display().to_string(),
        &prompt_paths,
        true,
        args.no_prompt_templates,
    );
    apply_extension_prompt_source_info(&mut templates, &extension_resources.prompt_resources, cwd);
    for diagnostic in &diagnostics {
        tracing::warn!(
            path = ?diagnostic.path,
            message = %diagnostic.message,
            "prompt template load diagnostic"
        );
    }
    templates
}

/// Build the faux model for the scripted test provider (shared by the run
/// and RPC paths).
pub fn build_faux_model(model_hint: Option<&str>) -> Result<pi_ai::model::Model, String> {
    let core = pi_ai::providers::FauxProviderCore::new(
        &pi_ai::providers::RegisterFauxProviderOptions::default(),
    );
    match model_hint {
        Some(hint) => {
            let id = hint.rsplit('/').next().unwrap_or(hint);
            core.get_model(Some(id))
                .cloned()
                .ok_or_else(|| format!("unknown faux model {id:?}"))
        }
        None => core
            .models
            .first()
            .cloned()
            .ok_or_else(|| "no faux model".to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::settings::SettingsMap;
    use serde_json::json;
    use std::sync::Arc;

    fn manager() -> SettingsManager {
        SettingsManager::in_memory(
            serde_json::from_value(json!({
                "defaultProvider": "faux",
                "defaultModel": "faux-1"
            }))
            .unwrap(),
        )
    }

    fn clear_env_provider_model() {
        unsafe {
            std::env::remove_var(config::ENV_PROVIDER);
            std::env::remove_var(config::ENV_MODEL);
        }
    }

    fn project_trust_runner(
        cwd: &str,
        handlers: Vec<crate::core::extensions::HandlerFn>,
    ) -> crate::core::extensions::ExtensionRunner {
        let runtime = crate::core::extensions::create_extension_runtime();
        let extension = crate::core::extensions::load_extension_from_factory(
            move |api| {
                for handler in handlers {
                    api.on("project_trust", handler)?;
                }
                Ok(())
            },
            cwd,
            runtime.clone(),
            "<inline:project-trust>",
        )
        .unwrap();
        let mut runner = crate::core::extensions::ExtensionRunner::new(
            vec![extension],
            runtime,
            cwd.to_string(),
        );
        runner.set_ui_context("print", false);
        runner
    }

    #[test]
    fn project_trust_bootstrap_extension_precedes_saved_and_default_and_remembers() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-project-trust-extension-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(cwd.join(crate::config::CONFIG_DIR_NAME)).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            cwd.join(crate::config::CONFIG_DIR_NAME)
                .join("settings.json"),
            "{}",
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"defaultProjectTrust":"never"}"#,
        )
        .unwrap();
        let store =
            crate::core::project_trust::ProjectTrustStore::new(&agent_dir.to_string_lossy());
        store.try_set(&cwd.to_string_lossy(), Some(false)).unwrap();
        let runner = project_trust_runner(
            &cwd.to_string_lossy(),
            vec![Arc::new(|_, _| {
                Ok(Some(json!({"trusted":"yes", "remember":true})))
            })],
        );

        assert_eq!(
            resolve_project_trust_without_prompt(
                &cwd.to_string_lossy(),
                &agent_dir,
                None,
                Some(&runner),
            ),
            Some(true)
        );
        assert_eq!(store.try_get(&cwd.to_string_lossy()).unwrap(), Some(true));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_trust_bootstrap_reports_error_then_accepts_later_decision() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-project-trust-errors-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(cwd.join(crate::config::CONFIG_DIR_NAME)).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            cwd.join(crate::config::CONFIG_DIR_NAME)
                .join("settings.json"),
            "{}",
        )
        .unwrap();
        let runner = project_trust_runner(
            &cwd.to_string_lossy(),
            vec![
                Arc::new(|_, _| Err("synthetic trust failure".to_string())),
                Arc::new(|_, _| Ok(Some(json!({"trusted":"no"})))),
            ],
        );

        assert_eq!(
            resolve_project_trust_without_prompt(
                &cwd.to_string_lossy(),
                &agent_dir,
                None,
                Some(&runner),
            ),
            Some(false)
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_trust_bootstrap_override_and_headless_fallback_are_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-project-trust-headless-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(cwd.join(crate::config::CONFIG_DIR_NAME)).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            cwd.join(crate::config::CONFIG_DIR_NAME)
                .join("settings.json"),
            "{}",
        )
        .unwrap();
        let runner = project_trust_runner(
            &cwd.to_string_lossy(),
            vec![Arc::new(|_, _| Ok(Some(json!({"trusted":"yes"}))))],
        );

        assert_eq!(
            resolve_project_trust_without_prompt(
                &cwd.to_string_lossy(),
                &agent_dir,
                Some(false),
                Some(&runner),
            ),
            Some(false)
        );
        assert_eq!(
            resolve_project_trust_without_prompt(&cwd.to_string_lossy(), &agent_dir, None, None,),
            None
        );
        assert!(!create_settings_with_project_trust(
            &cwd.to_string_lossy(),
            &agent_dir,
            None,
            false,
        )
        .is_project_trusted());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_trust_bootstrap_remember_failure_is_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-project-trust-remember-failure-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(cwd.join(crate::config::CONFIG_DIR_NAME)).unwrap();
        std::fs::create_dir_all(agent_dir.join("trust.json")).unwrap();
        std::fs::write(
            cwd.join(crate::config::CONFIG_DIR_NAME)
                .join("settings.json"),
            "{}",
        )
        .unwrap();
        let runner = project_trust_runner(
            &cwd.to_string_lossy(),
            vec![Arc::new(|_, _| {
                Ok(Some(json!({"trusted":"yes", "remember":true})))
            })],
        );

        assert_eq!(
            resolve_project_trust_without_prompt(
                &cwd.to_string_lossy(),
                &agent_dir,
                None,
                Some(&runner),
            ),
            Some(false)
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_project_trust_fails_closed_without_aborting_startup() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-project-trust-malformed-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(cwd.join(crate::config::CONFIG_DIR_NAME)).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            cwd.join(crate::config::CONFIG_DIR_NAME)
                .join("settings.json"),
            "{}",
        )
        .unwrap();
        std::fs::write(agent_dir.join("trust.json"), "{not-json").unwrap();

        let settings =
            create_settings_with_project_trust(&cwd.to_string_lossy(), &agent_dir, None, false);
        assert!(!settings.is_project_trusted());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn requested_thinking_level_prefers_cli_over_env_over_settings() {
        // CLI wins over everything; empty CLI values fall through.
        let resolved = resolve_requested_thinking_level(
            Some("high"),
            Some("low"),
            Some("off"),
            Some("minimal"),
            "medium",
        );
        assert_eq!(resolved.level, "high");
        assert!(resolved.warning.is_none());

        // Model-hint scope beats the environment.
        let resolved =
            resolve_requested_thinking_level(None, Some("low"), Some("high"), None, "medium");
        assert_eq!(resolved.level, "low");
        assert!(resolved.warning.is_none());

        // The environment beats the settings default.
        let resolved =
            resolve_requested_thinking_level(None, None, Some("high"), Some("low"), "medium");
        assert_eq!(resolved.level, "high");
        assert!(resolved.warning.is_none());

        // Empty values at every tier fall through to the builtin default.
        let resolved =
            resolve_requested_thinking_level(Some(""), Some(""), Some(""), Some(""), "medium");
        assert_eq!(resolved.level, "medium");
        assert!(resolved.warning.is_none());
        let resolved = resolve_requested_thinking_level(None, None, None, None, "medium");
        assert_eq!(resolved.level, "medium");
        assert!(resolved.warning.is_none());

        // An invalid environment value warns (CLI message shape) and falls
        // through to the settings default.
        let resolved =
            resolve_requested_thinking_level(None, None, Some("ultra"), Some("low"), "medium");
        assert_eq!(resolved.level, "low");
        assert!(
            resolved
                .warning
                .as_deref()
                .unwrap()
                .contains("Invalid PI_REASONING_LEVEL \"ultra\""),
            "unexpected warning: {:?}",
            resolved.warning
        );
    }

    #[test]
    fn request_api_key_ignores_empty_values_and_prefers_cli() {
        let args = Args {
            api_key: Some("cli-secret".to_string()),
            ..Default::default()
        };
        assert_eq!(
            request_api_key(&args, Some("environment-secret".to_string())).as_deref(),
            Some("cli-secret")
        );

        let args = Args {
            api_key: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            request_api_key(&args, Some("environment-secret".to_string())).as_deref(),
            Some("environment-secret")
        );
        assert!(request_api_key(&args, Some(String::new())).is_none());
    }

    #[test]
    fn retry_settings_feed_agent_and_provider_transport_options() {
        let settings = SettingsManager::in_memory(
            serde_json::from_value(json!({
                "transport": "sse",
                "httpIdleTimeoutMs": 4321,
                "websocketConnectTimeoutMs": 8765,
                "retry": {
                    "enabled": false,
                    "maxRetries": 4,
                    "baseDelayMs": 17,
                    "provider": {
                        "timeoutMs": 1234,
                        "maxRetries": 5,
                        "maxRetryDelayMs": 888
                    }
                }
            }))
            .unwrap(),
        );

        assert_eq!(
            retry_policy_from_settings(&settings),
            pi_ai::utils::RetryPolicy {
                enabled: false,
                max_retries: 4,
                base_delay_ms: 17,
            }
        );
        let options = stream_options_from_settings(&settings, Some("synthetic".to_string()));
        assert_eq!(options.base.api_key.as_deref(), Some("synthetic"));
        assert_eq!(options.base.timeout_ms, Some(1234));
        assert_eq!(options.base.max_retries, Some(5));
        assert_eq!(options.base.max_retry_delay_ms, Some(888));
        assert_eq!(options.transport.as_deref(), Some("sse"));
        assert_eq!(options.websocket_connect_timeout_ms, Some(8765));
    }

    #[test]
    fn empty_provider_and_model_values_are_not_explicit_sources() {
        assert_eq!(nonempty_value(None), None);
        assert_eq!(nonempty_value(Some("")), None);
        assert_eq!(nonempty_value(Some(" faux ")), Some(" faux "));
    }

    #[test]
    fn resolve_provider_cli_beats_settings() {
        clear_env_provider_model();
        let settings = SettingsManager::in_memory(
            serde_json::from_value(json!({
                "defaultProvider": "faux"
            }))
            .unwrap(),
        );
        let provider = resolve_run_provider(Some("anthropic"), None, &settings);
        assert_eq!(provider, "anthropic");
        let provider = resolve_run_provider(None, None, &settings);
        // An authenticated built-in provider may supersede this synthetic
        // test-only saved default; the resolver must still return a usable
        // provider rather than allowing an unauthenticated default to win.
        assert!(!provider.is_empty());
        let provider =
            resolve_run_provider(None, None, &SettingsManager::in_memory(SettingsMap::new()));
        assert!(!provider.is_empty());
    }

    #[test]
    fn resolve_model_settings_applies_when_no_explicit_provider() {
        clear_env_provider_model();
        let settings = manager();
        let model = resolve_run_model(None, &settings, true, Some("faux"));
        assert_eq!(model.as_deref(), Some("faux-1"));
        let model = resolve_run_model(Some("faux-2"), &settings, true, Some("faux"));
        assert_eq!(model.as_deref(), Some("faux-2"));
    }

    #[test]
    fn resolve_model_settings_do_not_cross_provider_fallback() {
        clear_env_provider_model();
        let settings = manager();
        let model = resolve_run_model(None, &settings, true, Some("qwen-token-plan"));
        assert_eq!(model, None);
    }

    #[test]
    fn resolve_model_settings_gated_off_with_explicit_provider() {
        clear_env_provider_model();
        let settings = manager();
        // Upstream pairs defaultProvider+defaultModel; an explicit provider
        // source means the settings default model must not leak in.
        let model = resolve_run_model(None, &settings, false, Some("faux"));
        assert_eq!(model, None);
        let model = resolve_run_model(Some("faux-2"), &settings, false, Some("faux"));
        assert_eq!(model.as_deref(), Some("faux-2"));
    }

    #[test]
    fn implicit_model_selection_requires_provider_auth() {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        models.set_provider(pi_ai::providers::google_provider_real());

        let error = require_authenticated_implicit_model(&models, "google", None)
            .expect_err("an unauthenticated implicit default must not be selected");
        assert!(error.starts_with("No models available."), "{error}");
        assert!(!error.contains("Provider is not configured"));

        assert!(
            require_authenticated_implicit_model(&models, "google", Some("gemini-2.5-flash"),)
                .is_ok()
        );
    }

    #[test]
    fn effective_model_patterns_prefer_cli_over_enabled_models() {
        let settings = SettingsManager::in_memory(
            serde_json::from_value(json!({
                "enabledModels": ["settings/*"]
            }))
            .unwrap(),
        );
        assert_eq!(
            effective_model_patterns(&Args::default(), &settings),
            vec!["settings/*"]
        );
        let models = vec![
            pi_ai::model::Model::new("settings-model", "Settings", "openai-chat", "settings"),
            pi_ai::model::Model::new("cli-model", "CLI", "openai-chat", "cli"),
        ];
        let (scoped, diagnostics) =
            resolve_effective_model_scope(&Args::default(), &settings, &models);
        assert!(diagnostics.is_empty());
        assert_eq!(scoped[0].model.provider, "settings");

        let args = Args {
            models: vec!["cli/*".to_string()],
            ..Default::default()
        };
        assert_eq!(effective_model_patterns(&args, &settings), vec!["cli/*"]);
        let (scoped, diagnostics) = resolve_effective_model_scope(&args, &settings, &models);
        assert!(diagnostics.is_empty());
        assert_eq!(scoped[0].model.provider, "cli");
    }

    #[test]
    fn session_root_precedence_matches_cli_env_settings_default() {
        let previous = std::env::var(config::ENV_SESSION_DIR).ok();
        unsafe {
            std::env::remove_var(config::ENV_SESSION_DIR);
        }

        let settings = SettingsManager::in_memory(
            serde_json::from_value(json!({"sessionDir": "/settings/sessions"})).unwrap(),
        );
        let args = Args::default();
        assert_eq!(
            resolve_session_root(&args, Some(&settings)),
            "/settings/sessions"
        );

        unsafe {
            std::env::set_var(config::ENV_SESSION_DIR, "/env/sessions");
        }
        assert_eq!(
            resolve_session_root(&args, Some(&settings)),
            "/env/sessions"
        );

        unsafe {
            std::env::set_var(config::ENV_SESSION_DIR, "");
        }
        assert_eq!(
            resolve_session_root(&Args::default(), Some(&settings)),
            "/settings/sessions"
        );

        let args = Args {
            session_dir: Some("/cli/sessions".to_string()),
            ..Args::default()
        };
        assert_eq!(
            resolve_session_root(&args, Some(&settings)),
            "/cli/sessions"
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var(config::ENV_SESSION_DIR, value),
                None => std::env::remove_var(config::ENV_SESSION_DIR),
            }
        }
    }

    #[test]
    fn session_name_normalization_collapses_line_break_runs() {
        assert_eq!(
            normalize_session_name_value("  first\r\n\nsecond\rthird  "),
            "first second third"
        );
        assert_eq!(normalize_session_name_value("\n\r\t"), "");
        assert_eq!(normalize_session_name_value("naïve λ"), "naïve λ");
    }

    #[tokio::test]
    async fn no_session_preserves_explicit_id_without_creating_storage() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-ephemeral-session-id-{}",
            uuid::Uuid::new_v4()
        ));
        let args = Args {
            no_session: true,
            session_id: Some("ephemeral-id".to_string()),
            ..Args::default()
        };

        let (session, path) = prepare_run_session(&args, &root.to_string_lossy())
            .await
            .unwrap();

        assert_eq!(session.get_metadata().await.id, "ephemeral-id");
        assert!(path.is_none());
        assert!(
            !root.exists(),
            "--no-session must not create a session root"
        );
    }

    #[tokio::test]
    async fn explicit_session_id_reopens_existing_project_session() {
        let root =
            std::env::temp_dir().join(format!("pi-run-session-id-reopen-{}", uuid::Uuid::new_v4()));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = root.to_string_lossy().into_owned();
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&cwd),
            sessions.to_string_lossy().into_owned(),
        );
        let existing = repo
            .create(CreateOptions {
                id: Some("restartable".to_string()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let existing_path = existing.get_metadata().await.path;

        let args = Args {
            session_dir: Some(sessions.to_string_lossy().into_owned()),
            session_id: Some("restartable".to_string()),
            ..Default::default()
        };
        let (reopened, path) = prepare_run_session(&args, &cwd).await.unwrap();
        assert_eq!(reopened.get_metadata().await.id, "restartable");
        assert_eq!(path.as_deref(), Some(existing_path.as_str()));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn selecting_session_with_missing_cwd_fails_before_noninteractive_run() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-session-missing-cwd-{}",
            uuid::Uuid::new_v4()
        ));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = root.to_string_lossy().into_owned();
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&cwd),
            sessions.to_string_lossy().into_owned(),
        );
        let missing_cwd = root.join("deleted-project");
        let source = repo
            .create(CreateOptions {
                id: Some("missing-cwd-session".to_string()),
                cwd: missing_cwd.to_string_lossy().into_owned(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();

        let args = Args {
            session: Some(source.get_metadata().await.id),
            session_dir: Some(sessions.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let error = match prepare_run_session(&args, &cwd).await {
            Ok(_) => panic!("missing session cwd unexpectedly resumed"),
            Err(error) => error,
        };
        let missing_cwd = missing_cwd.to_string_lossy();
        assert!(error.contains("Stored session working directory does not exist"));
        assert!(error.contains(missing_cwd.as_ref()));
        assert!(error.contains("Current working directory"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn session_selector_prefers_local_exact_id_before_global_matches() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-session-selector-precedence-{}",
            uuid::Uuid::new_v4()
        ));
        let sessions = root.join("sessions");
        let local = root.join("local");
        let foreign = root.join("foreign");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&foreign).unwrap();
        let local = local.to_string_lossy().into_owned();
        let foreign = foreign.to_string_lossy().into_owned();
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&local),
            sessions.to_string_lossy().into_owned(),
        );
        let local_session = repo
            .create(CreateOptions {
                id: Some("shared-id".to_string()),
                cwd: local.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let foreign_session = repo
            .create(CreateOptions {
                id: Some("shared-id".to_string()),
                cwd: foreign,
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();

        let resolved = resolve_session_metadata(&repo, "shared-id", &local)
            .await
            .unwrap();
        assert_eq!(
            resolved.path,
            local_session.get_metadata().await.path,
            "current-project exact id must win over the global inventory"
        );
        assert_ne!(resolved.path, foreign_session.get_metadata().await.path);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_selector_path_resolves_tilde_absolute_and_relative_values() {
        assert_eq!(
            resolve_session_selector_path("nested/session.jsonl", "/tmp/project"),
            PathBuf::from("/tmp/project/nested/session.jsonl")
        );
        assert_eq!(
            resolve_session_selector_path("nested/../session.jsonl", "/tmp/project"),
            PathBuf::from("/tmp/project/session.jsonl")
        );
        assert_eq!(
            resolve_session_selector_path("/tmp/session.jsonl", "/tmp/project"),
            PathBuf::from("/tmp/session.jsonl")
        );
        assert_eq!(
            resolve_session_selector_path("file:///tmp/session.jsonl", "/tmp/project"),
            PathBuf::from("/tmp/session.jsonl")
        );
    }

    #[tokio::test]
    async fn fork_rejects_an_existing_local_destination_id() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-session-id-fork-conflict-{}",
            uuid::Uuid::new_v4()
        ));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = root.to_string_lossy().into_owned();
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&cwd),
            sessions.to_string_lossy().into_owned(),
        );
        let source = repo
            .create(CreateOptions {
                id: Some("fork-source".to_string()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let destination = repo
            .create(CreateOptions {
                id: Some("fork-destination".to_string()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();

        let args = Args {
            session_dir: Some(sessions.to_string_lossy().into_owned()),
            fork: Some(source.get_metadata().await.path),
            session_id: Some("fork-destination".to_string()),
            ..Default::default()
        };
        let error = match prepare_run_session(&args, &cwd).await {
            Ok(_) => panic!("fork unexpectedly overwrote an existing destination"),
            Err(error) => error,
        };
        assert_eq!(error, "Session already exists with id 'fork-destination'");
        assert_eq!(destination.get_metadata().await.id, "fork-destination");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_skills_block_lists_loaded_skills() {
        let root = std::env::temp_dir().join(format!("pi-run-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let agent = root.join("agent");
        std::fs::create_dir_all(agent.join("skills/my-skill")).unwrap();
        std::fs::write(
            agent.join("skills/my-skill/SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill\n---\nbody",
        )
        .unwrap();
        let cwd = root.to_string_lossy().into_owned();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args::default();
        let block = build_skills_block(
            &args,
            &cwd,
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(block.contains("<available_skills>"));
        assert!(block.contains("<name>my-skill</name>"));
        assert!(!block.contains("disabled"), "no disabled skill");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_skills_block_empty_on_no_skills() {
        let root = std::env::temp_dir().join(format!("pi-run-noskills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args {
            no_skills: true,
            ..Default::default()
        };
        let block = build_skills_block(
            &args,
            &root.to_string_lossy(),
            &root,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert_eq!(block, "");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_skills_block_no_skills_keeps_explicit_skill_path() {
        let root =
            std::env::temp_dir().join(format!("pi-run-noskills-explicit-{}", uuid::Uuid::new_v4()));
        let skill_dir = root.join("explicit");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: explicit\ndescription: Explicit skill\n---\nbody",
        )
        .unwrap();
        let args = Args {
            no_skills: true,
            skills: vec![skill_dir.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let block = build_skills_block(
            &args,
            &root.to_string_lossy(),
            &root.join("agent"),
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(
            block.contains("<name>explicit</name>"),
            "-ns must retain an explicit --skill path: {block}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extension_resource_metadata_reaches_loaded_skill_and_prompt() {
        let root =
            std::env::temp_dir().join(format!("pi-run-resource-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let extension_dir = root.join(".pi/extensions");
        let skill_dir = extension_dir.join("skills/demo");
        let prompt_dir = extension_dir.join("prompts");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::create_dir_all(&prompt_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        let prompt_path = prompt_dir.join("demo.md");
        std::fs::write(
            &skill_path,
            "---\nname: demo\ndescription: extension skill\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            &prompt_path,
            "---\ndescription: extension prompt\n---\nbody",
        )
        .unwrap();

        let extension_path = extension_dir.join("example.js");
        let resource = crate::core::extensions::DiscoveredResource {
            path: "skills".to_string(),
            extension_path: extension_path.display().to_string(),
            source_info: crate::core::extensions::SourceInfo {
                path: extension_path.display().to_string(),
                source: "extension:example".to_string(),
                scope: "temporary".to_string(),
                origin: "top-level".to_string(),
                base_dir: Some(extension_dir.display().to_string()),
            },
        };
        let prompt_resource = crate::core::extensions::DiscoveredResource {
            path: "prompts".to_string(),
            ..resource.clone()
        };
        let cwd = root.display().to_string();
        let agent_dir = root.join("agent");
        let args = Args::default();
        let mut skill_resources = crate::core::extensions::ResourceDiscovery::default();
        skill_resources.skill_resources.push(resource);
        skill_resources.prompt_resources.push(prompt_resource);

        let mut skills = crate::core::skills::load_skills(crate::core::skills::LoadSkillsOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.display().to_string(),
            skill_paths: skill_resources.resolved_skill_paths(&cwd),
        })
        .0;
        apply_extension_skill_source_info(&mut skills, &skill_resources.skill_resources, &cwd);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source_info.source, "extension:example");
        assert_eq!(skills[0].source_info.scope, "temporary");
        assert_eq!(skills[0].source_info.path, skill_path.display().to_string());

        let templates = load_prompt_templates_for_run(&args, &cwd, &agent_dir, &skill_resources);
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].source_info.source, "extension:example");
        assert_eq!(
            templates[0].source_info.path,
            prompt_path.display().to_string()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn run_prompt_loader_includes_project_and_global_configured_paths() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-configured-prompts-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent = root.join("agent");
        let project_prompt = root.join("project-prompt.md");
        let global_prompt = root.join("global-prompt.md");
        std::fs::create_dir_all(cwd.join(crate::config::CONFIG_DIR_NAME)).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            &project_prompt,
            "---\ndescription: Project configured\n---\nproject: $@",
        )
        .unwrap();
        std::fs::write(
            &global_prompt,
            "---\ndescription: Global configured\n---\nglobal: $@",
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            serde_json::json!({
                "prompts": [global_prompt.to_string_lossy()],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            cwd.join(crate::config::CONFIG_DIR_NAME)
                .join("settings.json"),
            serde_json::json!({
                "prompts": [project_prompt.to_string_lossy()],
            })
            .to_string(),
        )
        .unwrap();
        let settings = SettingsManager::create(
            &cwd.to_string_lossy(),
            &agent.to_string_lossy(),
            crate::core::settings::SettingsManagerCreateOptions::default(),
        );
        let templates = load_prompt_templates_for_run_with_settings(
            &Args::default(),
            &cwd.to_string_lossy(),
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );

        assert!(templates.iter().any(|template| {
            template.name == "global-prompt" && template.content.contains("global: $@")
        }));
        assert!(templates.iter().any(|template| {
            template.name == "project-prompt" && template.content.contains("project: $@")
        }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_prompt_input_reads_file_or_passes_through() {
        let root = std::env::temp_dir().join(format!("pi-run-promptin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("append.md");
        std::fs::write(&file, "appended content").unwrap();
        assert_eq!(
            resolve_prompt_input(&file.to_string_lossy(), "append system prompt"),
            "appended content"
        );
        assert_eq!(
            resolve_prompt_input("inline text", "append system prompt"),
            "inline text"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovered_system_prompt_obeys_cli_trust_and_project_precedence() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-discovered-system-prompt-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        let project_pi = cwd.join(crate::config::CONFIG_DIR_NAME);
        std::fs::create_dir_all(&project_pi).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(project_pi.join("SYSTEM.md"), "trusted project system").unwrap();
        std::fs::write(agent_dir.join("SYSTEM.md"), "global system").unwrap();

        let mut settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args {
            no_context_files: true,
            no_skills: true,
            ..Default::default()
        };
        let project_prompt = assemble_run_system_prompt(
            &args,
            &cwd.to_string_lossy(),
            &agent_dir,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(project_prompt.starts_with("trusted project system"));
        assert!(!project_prompt.contains("global system"));

        settings.set_project_trusted(false);
        let global_prompt = assemble_run_system_prompt(
            &args,
            &cwd.to_string_lossy(),
            &agent_dir,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(global_prompt.starts_with("global system"));
        assert!(!global_prompt.contains("trusted project system"));

        let cli_args = Args {
            system_prompt: Some("cli system".to_string()),
            no_context_files: true,
            no_skills: true,
            ..Default::default()
        };
        let cli_prompt = assemble_run_system_prompt(
            &cli_args,
            &cwd.to_string_lossy(),
            &agent_dir,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(cli_prompt.starts_with("cli system"));
        assert!(!cli_prompt.contains("trusted project system"));
        assert!(!cli_prompt.contains("global system"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovered_append_system_prompt_obeys_cli_trust_and_precedence() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-discovered-append-prompt-{}",
            uuid::Uuid::new_v4()
        ));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        let project_pi = cwd.join(crate::config::CONFIG_DIR_NAME);
        std::fs::create_dir_all(&project_pi).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            project_pi.join("APPEND_SYSTEM.md"),
            "trusted project append",
        )
        .unwrap();
        std::fs::write(agent_dir.join("APPEND_SYSTEM.md"), "global append").unwrap();

        let mut settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args {
            system_prompt: Some("base".to_string()),
            no_context_files: true,
            no_skills: true,
            ..Default::default()
        };
        let project_prompt = assemble_run_system_prompt(
            &args,
            &cwd.to_string_lossy(),
            &agent_dir,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(project_prompt.contains("trusted project append"));
        assert!(!project_prompt.contains("global append"));

        settings.set_project_trusted(false);
        let global_prompt = assemble_run_system_prompt(
            &args,
            &cwd.to_string_lossy(),
            &agent_dir,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(global_prompt.contains("global append"));
        assert!(!global_prompt.contains("trusted project append"));

        let explicit_args = Args {
            system_prompt: Some("base".to_string()),
            append_system_prompt: vec!["explicit append".to_string()],
            no_context_files: true,
            no_skills: true,
            ..Default::default()
        };
        let explicit_prompt = assemble_run_system_prompt(
            &explicit_args,
            &cwd.to_string_lossy(),
            &agent_dir,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(explicit_prompt.contains("explicit append"));
        assert!(!explicit_prompt.contains("trusted project append"));
        assert!(!explicit_prompt.contains("global append"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn repeated_append_prompts_preserve_order_and_strip_bom() {
        let root =
            std::env::temp_dir().join(format!("pi-run-repeated-append-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let append_file = root.join("append.md");
        std::fs::write(&append_file, "\u{feff}from file").unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args {
            system_prompt: Some("base λ".to_string()),
            append_system_prompt: vec![
                "first".to_string(),
                append_file.to_string_lossy().into_owned(),
                String::new(),
                "last".to_string(),
            ],
            ..Default::default()
        };

        let prompt = assemble_run_system_prompt(
            &args,
            &root.to_string_lossy(),
            &root.join("agent"),
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        let first = prompt.find("first").unwrap();
        let from_file = prompt.find("from file").unwrap();
        let last = prompt.find("last").unwrap();
        assert!(first < from_file && from_file < last);
        assert!(!prompt.contains('\u{feff}'));
        assert!(!prompt.contains("\n\n\nlast"));
        assert!(prompt.ends_with(&format!(
            "Current working directory: {}\n",
            root.to_string_lossy().replace('\\', "/")
        )));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn assemble_system_prompt_preserves_upstream_section_order_and_nc() {
        let root = std::env::temp_dir().join(format!("pi-run-assemble-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let agent = root.join("agent");
        let cwd = root.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "project ctx line").unwrap();
        std::fs::create_dir_all(agent.join("skills/demo")).unwrap();
        std::fs::write(
            agent.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nbody",
        )
        .unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());

        let args = Args {
            system_prompt: Some("custom base".to_string()),
            append_system_prompt: vec!["tail".to_string()],
            tools: Some(vec!["read".to_string()]),
            ..Default::default()
        };
        let prompt = assemble_run_system_prompt(
            &args,
            &cwd.to_string_lossy(),
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        let append_index = prompt.find("tail").unwrap();
        let context_index = prompt.find("<project_context>").unwrap();
        let skills_index = prompt.find("<available_skills>").unwrap();
        let cwd_index = prompt.find("Current working directory:").unwrap();
        assert!(prompt.starts_with("custom base"));
        assert!(prompt.contains("project ctx line"));
        assert!(append_index < context_index);
        assert!(context_index < skills_index);
        assert!(skills_index < cwd_index);
        assert!(prompt.ends_with(&format!(
            "Current working directory: {}\n",
            cwd.to_string_lossy().replace('\\', "/")
        )));

        let args_nc = Args {
            no_context_files: true,
            ..Default::default()
        };
        let prompt_nc = assemble_run_system_prompt(
            &args_nc,
            &cwd.to_string_lossy(),
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(
            !prompt_nc.contains("<project_instructions"),
            "-nc must skip context files"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn assemble_system_prompt_default_lists_only_active_builtin_tools() {
        let root =
            std::env::temp_dir().join(format!("pi-run-prompt-default-{}", uuid::Uuid::new_v4()));
        let agent = root.join("agent");
        let cwd = root.join("project");
        std::fs::create_dir_all(agent.join("skills/demo")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            agent.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nbody",
        )
        .unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());

        let prompt = assemble_run_system_prompt(
            &Args::default(),
            &cwd.to_string_lossy(),
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        let read = prompt.find("- read: Read file contents").unwrap();
        let bash = prompt.find("- bash: Execute bash commands").unwrap();
        let edit = prompt.find("- edit: Make precise file edits").unwrap();
        let write = prompt.find("- write: Create or overwrite files").unwrap();
        assert!(read < bash && bash < edit && edit < write);
        assert!(!prompt.contains("- grep:"));
        assert!(!prompt.contains("- find:"));
        assert!(!prompt.contains("- ls:"));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("Use read to examine files instead of cat or sed."));
        assert!(prompt.ends_with(&format!(
            "Current working directory: {}",
            cwd.to_string_lossy().replace('\\', "/")
        )));

        let settings_default = SettingsManager::in_memory(
            serde_json::from_value(json!({"defaultTools": ["grep"]})).unwrap(),
        );
        let configured = assemble_run_system_prompt(
            &Args::default(),
            &cwd.to_string_lossy(),
            &agent,
            &settings_default,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(configured.contains("- grep: Search file contents for patterns"));
        assert!(!configured.contains("- read: Read file contents"));
        assert!(!configured.contains("<available_skills>"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn assemble_system_prompt_allowlist_and_exclusion_are_truthful() {
        let root =
            std::env::temp_dir().join(format!("pi-run-prompt-policy-{}", uuid::Uuid::new_v4()));
        let agent = root.join("agent");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let args = Args {
            tools: Some(vec![
                "grep".to_string(),
                "read".to_string(),
                "bash".to_string(),
            ]),
            exclude_tools: Some(vec!["read".to_string(), "bash".to_string()]),
            ..Default::default()
        };

        let prompt = assemble_run_system_prompt(
            &args,
            &cwd.to_string_lossy(),
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(prompt.contains("- grep: Search file contents for patterns"));
        assert!(!prompt.contains("- read: Read file contents"));
        assert!(!prompt.contains("- bash: Execute bash commands"));
        assert!(!prompt.contains("Use read to examine files instead of cat or sed."));
        assert!(!prompt.contains("You can inspect PI_* environment variables"));
        assert!(!prompt.contains("<available_skills>"));

        let no_tools = assemble_run_system_prompt(
            &Args {
                no_tools: true,
                ..Default::default()
            },
            &cwd.to_string_lossy(),
            &agent,
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
        );
        assert!(no_tools.contains("Available tools:\n(none)"));
        assert!(!no_tools.contains("<available_skills>"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn active_tool_policy_matches_cli_precedence_and_extension_boundary() {
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let available = ["read", "bash", "write", "extension_tool"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let explicit = Args {
            tools: Some(vec![
                "extension_tool".to_string(),
                "read".to_string(),
                "extension_tool".to_string(),
            ]),
            exclude_tools: Some(vec!["read".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            active_tool_names_for_policy(&explicit, &settings, &available),
            vec!["extension_tool"]
        );

        let no_builtin = Args {
            no_builtin_tools: true,
            ..Default::default()
        };
        assert_eq!(
            active_tool_names_for_policy(&no_builtin, &settings, &available),
            vec!["extension_tool"]
        );

        let no_tools = Args {
            no_tools: true,
            ..Default::default()
        };
        assert!(active_tool_names_for_policy(&no_tools, &settings, &available).is_empty());

        for mut suppression in [
            Args {
                no_tools: true,
                ..Default::default()
            },
            Args {
                no_builtin_tools: true,
                ..Default::default()
            },
        ] {
            suppression.tools = Some(vec!["read".to_owned(), "extension_tool".to_owned()]);
            assert_eq!(
                active_tool_names_for_policy(&suppression, &settings, &available),
                vec!["read", "extension_tool"]
            );
            assert!(should_register_builtin_tools(&suppression));
            assert!(should_register_extension_tools(&suppression));
        }
    }

    #[test]
    fn assemble_system_prompt_includes_active_extension_contributions() {
        let root =
            std::env::temp_dir().join(format!("pi-run-prompt-extension-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let extension = crate::core::extensions::RegisteredTool {
            name: "custom".to_string(),
            prompt_snippet: Some("  Custom action\nfor projects  ".to_string()),
            prompt_guidelines: Some(vec![
                "  Use custom only when needed.  ".to_string(),
                "Use custom only when needed.".to_string(),
            ]),
            ..Default::default()
        };
        let active = vec!["custom".to_string()];

        let prompt = assemble_system_prompt_from_active_tool_names(
            &Args::default(),
            &cwd.to_string_lossy(),
            &root.join("agent"),
            &settings,
            &crate::core::extensions::ResourceDiscovery::default(),
            &active,
            &[extension],
        );
        assert!(prompt.contains("- custom: Custom action for projects"));
        assert_eq!(prompt.matches("- Use custom only when needed.").count(), 1);
        assert!(!prompt.contains("- read: Read file contents"));
        assert!(!prompt.contains("<available_skills>"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn initial_fork_and_resume_honor_extension_cancellation() {
        let root = std::env::temp_dir().join(format!(
            "pi-run-initial-session-hooks-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cwd = root.to_string_lossy().into_owned();
        let session_dir = root.join("sessions");
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&cwd),
            session_dir.to_string_lossy().into_owned(),
        );
        let source = repo
            .create(CreateOptions {
                id: Some("source".to_string()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let source_path = source.get_metadata().await.path;
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_handler = Arc::clone(&events);
        let handler = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, event: &serde_json::Value| {
                events_for_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(Some(serde_json::json!({"cancel": true})))
            },
        ) as crate::core::extensions::HandlerFn;
        let mut extension = crate::core::extensions::Extension {
            path: "initial-session-hooks.js".to_string(),
            ..Default::default()
        };
        extension.handlers.insert(
            "session_before_fork".to_string(),
            vec![Arc::clone(&handler)],
        );
        extension
            .handlers
            .insert("session_before_switch".to_string(), vec![handler]);
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        let runner = crate::core::extensions::ExtensionRunner::new(
            vec![extension],
            extension_runtime,
            cwd.clone(),
        );

        let fork_args = Args {
            session_dir: Some(session_dir.to_string_lossy().into_owned()),
            fork: Some(source_path.clone()),
            ..Default::default()
        };
        let fork_result = prepare_run_session_with_lifecycle(&fork_args, &cwd, Some(&runner)).await;
        assert!(matches!(
            fork_result,
            Err(error) if error.contains("initial session fork cancelled")
        ));

        let resume_args = Args {
            session_dir: Some(session_dir.to_string_lossy().into_owned()),
            resume: true,
            ..Default::default()
        };
        let resume_result =
            prepare_run_session_with_lifecycle(&resume_args, &cwd, Some(&runner)).await;
        assert!(matches!(
            resume_result,
            Err(error) if error.contains("initial session resume cancelled")
        ));

        let events = events.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(events[0]["type"], "session_before_fork");
        assert_eq!(events[0]["entryId"], "source");
        assert_eq!(events[0]["position"], "at");
        assert_eq!(events[1]["type"], "session_before_switch");
        assert_eq!(events[1]["reason"], "resume");
        assert_eq!(events[1]["targetSessionFile"], source_path);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_arguments_attach_images_and_tag_text_references() {
        let root = std::env::temp_dir().join(format!("pi-run-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("prompt.md"), "inspect this").unwrap();

        let mut bmp = vec![0u8; 58];
        bmp[0..2].copy_from_slice(b"BM");
        let bmp_len = bmp.len() as u32;
        bmp[2..6].copy_from_slice(&bmp_len.to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        bmp[34..38].copy_from_slice(&4u32.to_le_bytes());
        bmp[56] = 0xff;
        std::fs::write(root.join("pixel.bmp"), bmp).unwrap();

        let cwd = root.to_string_lossy().to_string();
        let files = vec!["prompt.md".to_string(), "pixel.bmp".to_string()];
        let (text, images) = prepare_file_arguments(&files, &cwd, false)
            .unwrap()
            .expect("file arguments should produce an initial prompt");
        assert!(text.contains("<file name=\"") && text.contains("inspect this"));
        assert!(text.contains("pixel.bmp"));
        assert!(matches!(
            images.as_slice(),
            [ContentBlock::Image { mime_type, .. }] if mime_type == "image/png"
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_arguments_reject_invalid_utf8_instead_of_replacing_bytes() {
        let root = std::env::temp_dir().join(format!("pi-run-invalid-utf8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("prompt.txt");
        std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();

        let error =
            prepare_file_arguments(&["prompt.txt".to_string()], &root.to_string_lossy(), false)
                .unwrap_err();
        assert!(error.starts_with("Error: Could not read file "));
        assert!(error.contains("invalid utf-8"));
        assert!(!error.contains("�"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn deferred_mode_wiring_keeps_fetch_and_cancel_hooks() {
        for mode in ["print", "interactive", "json", "rpc"] {
            let models =
                pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
            let core = crate::core::model_runtime::register_faux_provider(
                &models,
                &pi_ai::providers::RegisterFauxProviderOptions {
                    deferred: Some(pi_ai::providers::FauxDeferredOptions {
                        pending_fetches: Some(0),
                        poll_after_ms: None,
                    }),
                    ..Default::default()
                },
            );
            core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
                pi_ai::providers::faux_assistant_message(
                    vec![ContentBlock::text(format!("{mode} deferred"))],
                    pi_ai::providers::FauxAssistantOptions::default(),
                ),
            )]);
            let provider = models.get_provider("faux").expect("faux provider");
            let streams = provider.single_streams.as_ref().expect("single API");
            assert!(streams.fetch_deferred.is_some(), "{mode} fetch hook");
            assert!(streams.cancel_deferred.is_some(), "{mode} cancel hook");
        }
    }
}
