//! `pi` binary entry point — port of `packages/coding-agent/src/main.ts`
//! (CLI dispatch; interactive/RPC modes arrive with pi-tui).

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;

use pi_coding_agent::args::{
    parse_args, parse_args_raw, print_help_with_extension_flags, print_version, Args,
    ExtensionFlagValue, ParseOutcome,
};

fn extension_flags_for_help(
    args: &pi_coding_agent::args::Args,
) -> Vec<pi_coding_agent::core::extensions::types::ExtensionFlag> {
    let cwd = pi_coding_agent::config::cwd();
    let agent_dir = pi_coding_agent::config::get_agent_dir();
    let settings = pi_coding_agent::run::create_mode_settings(args, &cwd, &agent_dir, false);
    let loaded = pi_coding_agent::core::extensions::load_for_mode_with_reason(
        args,
        &settings,
        &cwd,
        &agent_dir.to_string_lossy(),
        "print",
        false,
        args.name.clone(),
        args.thinking
            .clone()
            .unwrap_or_else(|| "medium".to_string()),
        "startup",
    );
    loaded.runner.get_flags().into_values().collect()
}

fn validate_extension_flags(
    args: &pi_coding_agent::args::Args,
    settings: &pi_coding_agent::core::settings::SettingsManager,
    cwd: &str,
    agent_dir: &str,
) -> Option<Vec<String>> {
    // The upstream parser stores unknown flags in a Map: repeating a name
    // replaces its value while retaining its first insertion position.
    let mut effective = Vec::new();
    for (name, value) in &args.extension_flag_values {
        if let Some(previous) = effective.iter_mut().find(|(key, _)| key == name) {
            previous.1 = value.clone();
        } else {
            effective.push((name.clone(), value.clone()));
        }
    }
    let values = effective
        .iter()
        .map(|(name, value)| {
            let value = match value {
                ExtensionFlagValue::Boolean(value) => serde_json::Value::Bool(*value),
                ExtensionFlagValue::String(value) => serde_json::Value::String(value.clone()),
            };
            (name.clone(), value)
        })
        .collect::<BTreeMap<_, _>>();
    let loaded = pi_coding_agent::core::extensions::load_for_mode_with_reason_and_flags(
        args,
        settings,
        cwd,
        agent_dir,
        "print",
        false,
        args.name.clone(),
        args.thinking
            .clone()
            .unwrap_or_else(|| "medium".to_string()),
        "startup",
        Some(values),
    );
    let flags = loaded.runner.get_flags();
    if flags.is_empty() {
        return None;
    }

    let mut diagnostics = Vec::new();
    let mut unknown = Vec::new();
    for (name, value) in &effective {
        let Some(flag) = flags.get(name) else {
            unknown.push(format!("--{name}"));
            continue;
        };
        if matches!(
            flag.flag_type,
            pi_coding_agent::core::extensions::FlagType::String
        ) && !matches!(value, ExtensionFlagValue::String(_))
        {
            diagnostics.push(format!("Extension flag \"--{name}\" requires a value"));
        }
    }
    if !unknown.is_empty() {
        diagnostics.push(format!(
            "Unknown option{}: {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown.join(", ")
        ));
    }
    Some(diagnostics)
}

fn report_parse_diagnostics(args: &pi_coding_agent::args::Args, suppress_empty_name: bool) -> bool {
    let mut has_error = false;
    for diagnostic in &args.diagnostics {
        if suppress_empty_name && diagnostic.message == "--name requires a non-empty value" {
            continue;
        }
        let label = match diagnostic.kind {
            pi_coding_agent::args::DiagnosticKind::Error => "Error",
            pi_coding_agent::args::DiagnosticKind::Warning => "Warning",
        };
        eprintln!("{label}: {}", diagnostic.message);
        has_error |= diagnostic.kind == pi_coding_agent::args::DiagnosticKind::Error;
    }
    has_error
}

/// Match `resolveAppMode` in the pinned upstream `main.ts`. Positional and
/// `@file` input do not force print mode when both standard streams are TTYs;
/// the interactive session consumes that input as its initial prompt.
fn interactive_requested(args: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> bool {
    args.mode.is_none()
        && !args.print
        && !args.list_models_requested()
        && stdin_is_tty
        && stdout_is_tty
}

const API_KEY_MODEL_ERROR: &str =
    "--api-key requires a model to be specified via --model, --provider/--model, or --models";

enum ProcessRunOutcome<T> {
    Completed(Result<T, String>),
    Signaled(i32),
}

/// Run a non-interactive mode with the same process-level shutdown contract as
/// upstream print-mode. Dropping the in-flight future first releases its
/// extension/runtime guards; the caller then exits with the conventional
/// signal status without printing a synthetic error line.
async fn run_with_process_signals<T, F>(future: F) -> ProcessRunOutcome<T>
where
    F: Future<Output = Result<T, String>>,
{
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    return ProcessRunOutcome::Completed(Err(format!(
                        "install SIGTERM handler: {error}"
                    )))
                }
            };
        let mut sighup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(signal) => signal,
            Err(error) => {
                return ProcessRunOutcome::Completed(Err(format!(
                    "install SIGHUP handler: {error}"
                )))
            }
        };
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => ProcessRunOutcome::Completed(result),
            Some(_) = sigterm.recv() => ProcessRunOutcome::Signaled(143),
            Some(_) = sighup.recv() => ProcessRunOutcome::Signaled(129),
        }
    }
    #[cfg(not(unix))]
    {
        ProcessRunOutcome::Completed(future.await)
    }
}

/// Upstream only applies the explicit `--api-key` branch for a non-empty
/// value. A provider by itself is not a model selection; an explicit model,
/// model scope, saved default, restored session, or the Rust port's
/// environment model is required before the request-scoped key can be
/// attached. The settings/session inputs are supplied by the caller because
/// upstream performs this check after those runtime selections are resolved.
fn api_key_requires_model(
    args: &Args,
    env_provider: Option<&str>,
    env_model: Option<&str>,
    settings: Option<&pi_coding_agent::core::settings::SettingsManager>,
) -> bool {
    let has_api_key = args
        .api_key
        .as_deref()
        .is_some_and(|api_key| !api_key.is_empty());
    if !has_api_key {
        return false;
    }

    // A resumed/session/forked runtime can restore its model from the session
    // before the upstream API-key check runs. Do not reject that valid source
    // at the parser boundary merely because the selector itself has no model
    // argument.
    if args.continue_session || args.resume || args.session.is_some() || args.fork.is_some() {
        return false;
    }

    let explicit_model = args.model.as_deref().is_some_and(|model| !model.is_empty())
        || args.models.iter().any(|pattern| !pattern.trim().is_empty())
        || env_model.is_some_and(|model| !model.is_empty());
    if explicit_model {
        return false;
    }

    // A saved default is also resolved before the upstream API-key branch.
    // Only accept it when an explicit provider source does not conflict with
    // the saved provider; the run path intentionally does not leak a saved
    // model into a different provider scope.
    let has_saved_default = settings.is_some_and(|settings| {
        let Some(provider) = settings.get_default_provider() else {
            return false;
        };
        let Some(model) = settings.get_default_model() else {
            return false;
        };
        if provider.is_empty() || model.is_empty() {
            return false;
        }
        env_provider
            .or(args.provider.as_deref())
            .is_none_or(|requested| requested.eq_ignore_ascii_case(provider))
    });
    !has_saved_default
}

/// Validate the session-selector combinations before any mode creates or
/// opens storage. This mirrors the pinned `validateForkFlags` and
/// `validateSessionIdFlags` caller checks, including allowing `--session-id`
/// as the destination id for `--fork`.
fn validate_session_flags(args: &Args) -> Result<(), String> {
    if args.fork.is_some() {
        let mut conflicts = Vec::new();
        if args.session.is_some() {
            conflicts.push("--session");
        }
        if args.continue_session {
            conflicts.push("--continue");
        }
        if args.resume {
            conflicts.push("--resume");
        }
        if args.no_session {
            conflicts.push("--no-session");
        }
        if !conflicts.is_empty() {
            return Err(format!(
                "--fork cannot be combined with {}",
                conflicts.join(", ")
            ));
        }
    }

    if let Some(session_id) = args.session_id.as_deref() {
        let mut conflicts = Vec::new();
        if args.session.is_some() {
            conflicts.push("--session");
        }
        if args.continue_session {
            conflicts.push("--continue");
        }
        if args.resume {
            conflicts.push("--resume");
        }
        if !conflicts.is_empty() {
            return Err(format!(
                "--session-id cannot be combined with {}",
                conflicts.join(", ")
            ));
        }
        pi_coding_agent::core::session_migration::assert_valid_session_id(session_id)?;
    }

    Ok(())
}

/// `normalizeSessionName` is a caller concern in the pinned CLI. Store the
/// normalized value so print, interactive, and JSON callers persist/display
/// the same name rather than retaining incidental edge whitespace.
fn normalize_session_name(args: &mut Args) -> Result<(), String> {
    if let Some(name) = args.name.as_mut() {
        let normalized = pi_coding_agent::run::normalize_session_name_value(name);
        if normalized.is_empty() {
            return Err("--name requires a non-empty value".to_string());
        }
        *name = normalized;
    }
    Ok(())
}

/// Apply the interactive-only `--use-theme` override without persisting it.
///
/// The pinned CLI applies this value to the startup settings view before the
/// interactive runtime is created. It is deliberately a run-scoped override:
/// selecting a theme on the command line must not rewrite `settings.json`, and
/// non-interactive modes do not consume the flag.
fn apply_interactive_theme_override(
    args: &Args,
    settings: &mut pi_coding_agent::core::settings::SettingsManager,
) {
    let Some(theme) = args.use_theme.as_deref() else {
        return;
    };
    let mut overrides = pi_coding_agent::core::settings::SettingsMap::new();
    overrides.insert(
        "theme".to_string(),
        serde_json::Value::String(theme.to_string()),
    );
    settings.apply_overrides(&overrides);
}

fn final_text_line(text: &str) -> String {
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    line
}

async fn write_final_text(text: &str) -> std::io::Result<()> {
    pi_coding_agent::core::output_guard::write_raw_stdout(&final_text_line(text)).await
}

#[tokio::main]
async fn main() {
    // Rust's `print!`/`println!` macros panic when a downstream pager or
    // `head` closes stdout.  CLI tools conventionally treat that as a clean
    // early exit; install the narrow process-level equivalent before any
    // command output is emitted so help/version/stream modes are pipe-safe.
    std::panic::set_hook(Box::new(|panic| {
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or_default();
        if message.contains("failed printing to stdout")
            && message.to_ascii_lowercase().contains("broken pipe")
        {
            std::process::exit(0);
        }
        eprintln!("{panic}");
    }));
    pi_coding_agent::core::timings::reset_timings("main");
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Bootstrap the global proxy before auth/package/config subcommands, which
    // can create provider HTTP clients without going through a mode settings
    // manager. Existing HTTP_PROXY/HTTPS_PROXY values always win.
    let agent_dir = pi_coding_agent::config::get_agent_dir();
    if let Err(error) =
        pi_coding_agent::core::http_dispatcher::apply_global_http_proxy_settings(&agent_dir)
    {
        eprintln!("Warning: {error}");
    }

    // Keep startup network behavior consistent across subcommands. The
    // upstream sets both switches before auth/package dispatch, so an
    // offline invocation never performs a version or catalog request.
    if argv.iter().any(|arg| arg == "--offline") || pi_coding_agent::config::env_flag("PI_OFFLINE")
    {
        std::env::set_var("PI_OFFLINE", "1");
        std::env::set_var("PI_SKIP_VERSION_CHECK", "1");
    }

    // Subcommand dispatch mirrors main.ts: auth commands, package commands,
    // and the config command run before generic arg parsing.
    if pi_coding_agent::commands::auth::handle_auth_command(&argv).await {
        pi_coding_agent::core::timings::time("authCommand", "main");
        pi_coding_agent::core::timings::print_timings();
        return;
    }
    if pi_coding_agent::commands::package::handle_package_command(&argv).await {
        pi_coding_agent::core::timings::time("packageCommand", "main");
        pi_coding_agent::core::timings::print_timings();
        return;
    }
    if pi_coding_agent::commands::config::handle_config_command(&argv) {
        pi_coding_agent::core::timings::time("configCommand", "main");
        pi_coding_agent::core::timings::print_timings();
        return;
    }

    // Experimental server/client commands are selected before the legacy
    // parser so a positional message named "server" remains ordinary only
    // when it is not the first argument (matching the upstream command tree).
    match pi_coding_agent::core::experimental::parse_command(&argv) {
        Ok(Some(command)) => {
            if let Err(error) = pi_coding_agent::run::run_experimental_command(command).await {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
            return;
        }
        Ok(None) => {}
        Err(errors) => {
            if argv
                .iter()
                .any(|argument| argument == "--help" || argument == "-h")
                && pi_coding_agent::core::experimental::are_enabled()
            {
                for error in errors {
                    println!("{error}");
                }
            } else {
                for error in errors {
                    eprintln!("Error: {error}");
                }
                std::process::exit(1);
            }
            return;
        }
    }

    let parsed_args = parse_args_raw(&argv);
    if report_parse_diagnostics(&parsed_args, parsed_args.version) {
        pi_coding_agent::core::timings::time("parseArgs", "main");
        pi_coding_agent::core::timings::print_timings();
        std::process::exit(1);
    }

    match parse_args(&argv) {
        ParseOutcome::Help => {
            pi_coding_agent::core::timings::time("parseArgs", "main");
            let extension_flags = extension_flags_for_help(&parsed_args);
            print_help_with_extension_flags(&extension_flags);
            pi_coding_agent::core::timings::print_timings();
        }
        ParseOutcome::Version => {
            pi_coding_agent::core::timings::time("parseArgs", "main");
            print_version();
            pi_coding_agent::core::timings::print_timings();
        }
        ParseOutcome::Run(mut args) => {
            pi_coding_agent::core::timings::time("parseArgs", "main");
            // Upstream handles export immediately after parsing, before mode
            // selection, stdin consumption, or list-models.  In particular,
            // an export request must not be reinterpreted as a JSON/RPC
            // startup or block waiting for piped input.
            if let Some(input_path) = args.export.clone() {
                let output_path = args.messages.first().map(String::as_str);
                match pi_coding_agent::core::export_html::export_session_file(
                    &input_path,
                    output_path,
                    None,
                ) {
                    Ok(path) => {
                        println!("Exported to: {path}");
                        pi_coding_agent::core::timings::time("export", "main");
                        pi_coding_agent::core::timings::print_timings();
                        return;
                    }
                    Err(err) => {
                        eprintln!("Error: {err}");
                        pi_coding_agent::core::timings::print_timings();
                        std::process::exit(1);
                    }
                }
            }
            if !args.unknown_flags.is_empty() {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                let settings =
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, false);
                let extension_diagnostics =
                    validate_extension_flags(&args, &settings, &cwd, &agent_dir.to_string_lossy());
                match extension_diagnostics {
                    Some(diagnostics) if diagnostics.is_empty() => {
                        // A registered flag is accepted here after native
                        // validation; its parsed value has been seeded into
                        // the flag-aware native startup runtime above.
                        args.unknown_flags.clear();
                    }
                    Some(diagnostics) => {
                        for diagnostic in diagnostics {
                            eprintln!("Error: {diagnostic}");
                        }
                        pi_coding_agent::core::timings::print_timings();
                        std::process::exit(1);
                    }
                    None => {
                        // With no registered extension flags retain the
                        // existing Rust-facing diagnostic.
                        println!();
                        eprintln!("Unknown flag: {}", args.unknown_flags.join(", "));
                        pi_coding_agent::core::timings::print_timings();
                        std::process::exit(1);
                    }
                }
            }
            if let Err(error) = validate_session_flags(&args) {
                eprintln!("Error: {error}");
                pi_coding_agent::core::timings::print_timings();
                std::process::exit(1);
            }
            if let Err(error) = normalize_session_name(&mut args) {
                eprintln!("Error: {error}");
                pi_coding_agent::core::timings::print_timings();
                std::process::exit(1);
            }
            let api_key_settings = args
                .api_key
                .as_deref()
                .filter(|api_key| !api_key.is_empty())
                .map(|_| {
                    let cwd = pi_coding_agent::config::cwd();
                    let agent_dir = pi_coding_agent::config::get_agent_dir();
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, false)
                });
            if api_key_requires_model(
                &args,
                pi_coding_agent::config::env("PI_PROVIDER").as_deref(),
                pi_coding_agent::config::env("PI_MODEL").as_deref(),
                api_key_settings.as_ref(),
            ) {
                eprintln!("Error: {API_KEY_MODEL_ERROR}");
                pi_coding_agent::core::timings::print_timings();
                std::process::exit(1);
            }
            // Match the upstream non-interactive entry path: ordinary and
            // JSON modes consume piped stdin as part of the initial prompt;
            // RPC reserves stdin for its JSONL command protocol.
            if args.mode.as_deref() != Some("rpc")
                && !std::io::IsTerminal::is_terminal(&std::io::stdin())
            {
                let mut stdin = std::io::stdin();
                let mut content = String::new();
                if let Err(error) = stdin.read_to_string(&mut content) {
                    eprintln!("Error: failed to read piped stdin: {error}");
                    std::process::exit(1);
                }
                let content = content.trim();
                if !content.is_empty() {
                    args.stdin_content = Some(content.to_string());
                }
            }
            if args.mode.as_deref() == Some("rpc") && !args.file_args.is_empty() {
                eprintln!("Error: @file arguments are not supported in RPC mode");
                std::process::exit(1);
            }
            let should_enter_interactive = interactive_requested(
                &args,
                std::io::IsTerminal::is_terminal(&std::io::stdin()),
                std::io::IsTerminal::is_terminal(&std::io::stdout()),
            );
            if pi_coding_agent::config::env_flag("PI_STARTUP_BENCHMARK")
                && !should_enter_interactive
            {
                eprintln!("Error: PI_STARTUP_BENCHMARK only supports interactive mode");
                pi_coding_agent::core::timings::print_timings();
                std::process::exit(1);
            }
            // Interactive TUI mode: no --print/--mode and a TTY stdin+mount.
            if should_enter_interactive {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                if let Err(err) =
                    pi_coding_agent::run::run_first_time_setup_if_needed(&cwd, &agent_dir).await
                {
                    eprintln!("startup setup error: {err}");
                    pi_coding_agent::core::timings::print_timings();
                    std::process::exit(1);
                }
                let mut settings =
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, true);
                apply_interactive_theme_override(&args, &mut settings);
                let result =
                    pi_coding_agent::modes::interactive::run_interactive_mode(&args, settings)
                        .await;
                if let Err(err) = result {
                    eprintln!("interactive error: {err}");
                    pi_coding_agent::core::timings::print_timings();
                    std::process::exit(1);
                }
                pi_coding_agent::core::timings::time("interactiveMode", "main");
                pi_coding_agent::core::timings::print_timings();
                return;
            }
            // --mode json: JSON event stream over stdout.
            if args.mode.as_deref() == Some("json") {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                let settings =
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, false);
                match run_with_process_signals(pi_coding_agent::modes::json_event::run_json_mode(
                    &args, settings,
                ))
                .await
                {
                    ProcessRunOutcome::Signaled(code) => std::process::exit(code),
                    ProcessRunOutcome::Completed(Ok(())) => {}
                    ProcessRunOutcome::Completed(Err(err)) => {
                        eprintln!("Error: {err}");
                        pi_coding_agent::core::timings::print_timings();
                        std::process::exit(1);
                    }
                }
                pi_coding_agent::core::timings::time("jsonMode", "main");
                pi_coding_agent::core::timings::print_timings();
                return;
            }
            // --mode rpc: headless JSONL protocol over stdio.
            if args.mode.as_deref() == Some("rpc") {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                let settings =
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, false);
                if let Err(err) = pi_coding_agent::modes::rpc::run_rpc_mode(&args, settings).await {
                    eprintln!("rpc error: {err}");
                    pi_coding_agent::core::timings::print_timings();
                    std::process::exit(1);
                }
                pi_coding_agent::core::timings::time("rpcMode", "main");
                pi_coding_agent::core::timings::print_timings();
                return;
            }
            // --list-models: build the built-in provider registry and print
            // the auth-gated model table (upstream list-models behavior).
            if args.list_models_requested() {
                // `listModels` runs against the same models.json-composed
                // runtime registry as ordinary startup. Using only the
                // bundled catalog silently omitted user-defined/overlaid
                // models from this command.
                let base = pi_coding_agent::core::model_registry::builtin_models();
                let (models, models_json_error, models_json_config) =
                    match pi_coding_agent::core::model_config::models_json_path() {
                        Some(path) => {
                            let config =
                                pi_coding_agent::core::model_config::ModelConfig::load(Some(&path));
                            let registry =
                                pi_coding_agent::core::model_registry::ModelRegistry::new(
                                    base,
                                    config.clone(),
                                );
                            (
                                registry.into_models(),
                                registry.get_error().map(str::to_owned),
                                Some(config),
                            )
                        }
                        None => (base, None, None),
                    };
                if let Some(error) = models_json_error {
                    eprintln!("Warning: errors loading models.json:\n{error}");
                }
                if let Some(config) = models_json_config {
                    for provider in config.get_provider_ids() {
                        if let Some(key) = config.get_provider(provider).and_then(
                            pi_coding_agent::core::provider_composer::resolve_models_json_api_key,
                        ) {
                            models.set_runtime_api_key(provider, key);
                        }
                    }
                }
                // llama.cpp is a hidden/native dynamic provider.  It is
                // intentionally lazy for ordinary startup, but an explicit
                // search for it must restore/refresh its real local catalog
                // before formatting the table.
                if args
                    .list_models
                    .as_deref()
                    .is_some_and(|pattern| pattern.to_ascii_lowercase().contains("llama"))
                {
                    if let Err(error) =
                        pi_coding_agent::core::model_runtime::register_llama_provider_if_selected(
                            &models,
                            pi_coding_agent::core::llama::LLAMA_PROVIDER_ID,
                            !args.offline
                                && !pi_coding_agent::config::env_flag(
                                    pi_coding_agent::config::ENV_OFFLINE,
                                ),
                        )
                        .await
                    {
                        eprintln!("warning: {error}");
                    }
                }
                let out =
                    pi_coding_agent::list_models::list_models(&models, args.list_models.as_deref());
                print!("{out}");
                pi_coding_agent::core::timings::time("listModels", "main");
                pi_coding_agent::core::timings::print_timings();
                return;
            }
            // An empty non-interactive invocation is a clean no-op. Resolve
            // providers only when there is work to run; otherwise a clean
            // environment with no credentials would report a misleading
            // model-selection error for commands such as `--mode invalid`.
            // This is also the official CLI's behavior after it prepares an
            // empty print-mode message.
            let has_no_initial_input = args.messages.is_empty()
                && args.stdin_content.is_none()
                && args.file_args.is_empty()
                && !args.print
                && !args.continue_session
                && !args.resume
                && args.session.is_none()
                && args.session_id.is_none()
                && args.fork.is_none()
                && !args
                    .api_key
                    .as_deref()
                    .is_some_and(|api_key| !api_key.is_empty())
                && args.system_prompt.is_none()
                && args.append_system_prompt.is_empty()
                && args.name.is_none();
            if has_no_initial_input {
                println!();
                pi_coding_agent::core::timings::time("run", "main");
                pi_coding_agent::core::timings::print_timings();
                return;
            }
            match run_with_process_signals(pi_coding_agent::run::run(&args)).await {
                ProcessRunOutcome::Signaled(code) => std::process::exit(code),
                ProcessRunOutcome::Completed(result) => match result {
                    Ok(outcome) => {
                        if let Err(error) = write_final_text(&outcome.final_text).await {
                            if error.kind() == std::io::ErrorKind::BrokenPipe {
                                return;
                            }
                            eprintln!("Error: stdout write failed: {error}");
                            pi_coding_agent::core::timings::print_timings();
                            std::process::exit(1);
                        }
                        pi_coding_agent::core::timings::time("run", "main");
                        pi_coding_agent::core::timings::print_timings();
                    }
                    Err(err) => {
                        eprintln!("{err}");
                        pi_coding_agent::core::timings::print_timings();
                        std::process::exit(1);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Args {
        parse_args_raw(
            &argv
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn positional_and_file_input_keep_tty_bootstrap_interactive() {
        assert!(interactive_requested(&args(&[]), true, true));
        assert!(interactive_requested(&args(&["hello"]), true, true));
        assert!(interactive_requested(&args(&["@prompt.md"]), true, true));
        assert!(!interactive_requested(
            &args(&["--print", "hello"]),
            true,
            true
        ));
        assert!(!interactive_requested(
            &args(&["--mode", "json"]),
            true,
            true
        ));
        assert!(!interactive_requested(&args(&[]), false, true));
        assert!(!interactive_requested(
            &args(&["--list-models"]),
            true,
            true
        ));
    }

    #[test]
    fn explicit_api_key_requires_a_nonempty_model_selection() {
        assert!(api_key_requires_model(
            &args(&["--api-key", "secret"]),
            None,
            None,
            None,
        ));
        assert!(!api_key_requires_model(
            &args(&["--api-key", ""]),
            None,
            None,
            None,
        ));
        assert!(!api_key_requires_model(
            &args(&["--api-key", "secret", "--model", "faux-1"]),
            None,
            None,
            None,
        ));
        assert!(!api_key_requires_model(
            &args(&["--api-key", "secret", "--models", "faux-*"]),
            None,
            None,
            None,
        ));
        assert!(!api_key_requires_model(
            &args(&["--api-key", "secret"]),
            None,
            Some("faux-1"),
            None,
        ));

        let settings = pi_coding_agent::core::settings::SettingsManager::in_memory(
            serde_json::from_value(serde_json::json!({
                "defaultProvider": "faux",
                "defaultModel": "faux-1",
            }))
            .unwrap(),
        );
        assert!(!api_key_requires_model(
            &args(&["--api-key", "secret"]),
            None,
            None,
            Some(&settings),
        ));
        assert!(api_key_requires_model(
            &args(&["--api-key", "secret", "--provider", "anthropic"]),
            None,
            None,
            Some(&settings),
        ));
        assert!(!api_key_requires_model(
            &args(&["--api-key", "secret", "--continue"]),
            None,
            None,
            None,
        ));
    }

    #[test]
    fn session_selector_conflicts_and_name_normalization_match_cli_contract() {
        let fork_and_resume = args(&["--fork", "source", "--resume"]);
        assert_eq!(
            validate_session_flags(&fork_and_resume).unwrap_err(),
            "--fork cannot be combined with --resume"
        );

        let session_and_id = args(&["--session", "source", "--session-id", "child-1"]);
        assert_eq!(
            validate_session_flags(&session_and_id).unwrap_err(),
            "--session-id cannot be combined with --session"
        );

        let fork_with_destination_id = args(&["--fork", "source", "--session-id", "child-1"]);
        assert!(validate_session_flags(&fork_with_destination_id).is_ok());

        let invalid_id = args(&["--session-id", "-bad"]);
        assert!(validate_session_flags(&invalid_id)
            .unwrap_err()
            .contains("Session id must be non-empty"));

        let mut named = args(&["--name", "  Café  "]);
        normalize_session_name(&mut named).unwrap();
        assert_eq!(named.name.as_deref(), Some("Café"));

        let mut multiline = args(&["hello"]);
        multiline.name = Some("  first\r\n\nsecond\rthird  ".to_string());
        normalize_session_name(&mut multiline).unwrap();
        assert_eq!(multiline.name.as_deref(), Some("first second third"));

        let mut empty_name = args(&["hello"]);
        empty_name.name = Some("   ".to_string());
        assert_eq!(
            normalize_session_name(&mut empty_name).unwrap_err(),
            "--name requires a non-empty value"
        );
    }

    #[test]
    fn use_theme_is_an_interactive_run_scoped_override() {
        let args = args(&["--use-theme", "solarized"]);
        let mut settings = pi_coding_agent::core::settings::SettingsManager::in_memory(
            pi_coding_agent::core::settings::SettingsMap::new(),
        );

        apply_interactive_theme_override(&args, &mut settings);

        assert_eq!(settings.get_theme_setting(), Some("solarized"));
        assert!(settings.get_global_settings().get("theme").is_none());
    }

    #[test]
    fn final_text_line_matches_print_mode_newline_contract() {
        assert_eq!(final_text_line("answer"), "answer\n");
        assert_eq!(final_text_line(""), "\n");
        assert_eq!(final_text_line("line\n"), "line\n\n");
    }
}
