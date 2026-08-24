//! `pi` binary entry point — port of `packages/coding-agent/src/main.ts`
//! (CLI dispatch; interactive/RPC modes arrive with pi-tui).

use pi_coding_agent::args::{parse_args, print_help, print_version, ParseOutcome};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Keep startup network behavior consistent across subcommands. The
    // upstream sets both switches before auth/package dispatch, so an
    // offline invocation never performs a version or catalog request.
    if argv.iter().any(|arg| arg == "--offline") || pi_coding_agent::config::env_flag("PI_OFFLINE")
    {
        std::env::set_var("PI_OFFLINE", "1");
        std::env::set_var("PI_SKIP_VERSION_CHECK", "1");
    }

    if let Some(notice) = pi_coding_agent::core::timings::startup_notice() {
        eprintln!("Warning: {notice}");
    }

    // Subcommand dispatch mirrors main.ts: auth commands, package commands,
    // and the config command run before generic arg parsing.
    if pi_coding_agent::commands::auth::handle_auth_command(&argv) {
        return;
    }
    if pi_coding_agent::commands::package::handle_package_command(&argv).await {
        return;
    }
    if pi_coding_agent::commands::config::handle_config_command(&argv) {
        return;
    }

    match parse_args(&argv) {
        ParseOutcome::Help => {
            print_help();
        }
        ParseOutcome::Version => {
            print_version();
        }
        ParseOutcome::Run(args) => {
            // Surface parse diagnostics (upstream main.ts): errors print
            // "Error:" and warnings "Warning:" to stderr — ALL are printed
            // first, then we exit 1 if any was an error (upstream prints the
            // full set before `process.exit(1)`).
            if !args.diagnostics.is_empty() {
                let any_error = args
                    .diagnostics
                    .iter()
                    .any(|d| d.kind == pi_coding_agent::args::DiagnosticKind::Error);
                for d in &args.diagnostics {
                    let label = match d.kind {
                        pi_coding_agent::args::DiagnosticKind::Error => "Error",
                        pi_coding_agent::args::DiagnosticKind::Warning => "Warning",
                    };
                    eprintln!("{label}: {}", d.message);
                }
                if any_error {
                    std::process::exit(1);
                }
            }
            if !args.unknown_flags.is_empty() {
                eprintln!("unknown flags: {}", args.unknown_flags.join(", "));
            }
            if args.mode.as_deref() == Some("rpc") && !args.file_args.is_empty() {
                eprintln!("Error: @file arguments are not supported in RPC mode");
                std::process::exit(1);
            }
            // Interactive TUI mode: no --print/--mode and a TTY stdin+mount.
            if args.mode.is_none()
                && !args.print
                && args.messages.is_empty()
                && std::io::IsTerminal::is_terminal(&std::io::stdin())
                && std::io::IsTerminal::is_terminal(&std::io::stdout())
            {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                let settings =
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, true);
                let result =
                    pi_coding_agent::modes::interactive::run_interactive_mode(&args, settings)
                        .await;
                if let Err(err) = result {
                    eprintln!("interactive error: {err}");
                    std::process::exit(1);
                }
                return;
            }
            // --mode json: JSON event stream over stdout.
            if args.mode.as_deref() == Some("json") {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                let settings =
                    pi_coding_agent::run::create_mode_settings(&args, &cwd, &agent_dir, false);
                if let Err(err) =
                    pi_coding_agent::modes::json_event::run_json_mode(&args, settings).await
                {
                    eprintln!("Error: {err}");
                    std::process::exit(1);
                }
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
                    std::process::exit(1);
                }
                return;
            }
            // --list-models: build the built-in provider registry and print
            // the auth-gated model table (upstream list-models behavior).
            if args.list_models_requested() {
                let models = pi_coding_agent::core::model_registry::builtin_models();
                let out =
                    pi_coding_agent::list_models::list_models(&models, args.list_models.as_deref());
                print!("{out}");
                return;
            }
            // --export <file> [output]: export a session JSONL file to HTML
            // and exit (upstream exportFromFile + "Exported to:" print).
            if let Some(input_path) = args.export.clone() {
                let output_path = args.messages.first().map(String::as_str);
                match pi_coding_agent::core::export_html::export_session_file(
                    &input_path,
                    output_path,
                    None,
                ) {
                    Ok(path) => {
                        println!("Exported to: {path}");
                        return;
                    }
                    Err(err) => {
                        eprintln!("Error: {err}");
                        std::process::exit(1);
                    }
                }
            }
            match pi_coding_agent::run::run(&args).await {
                Ok(outcome) => {
                    println!("{}", outcome.final_text);
                    if let Some(path) = &outcome.session_path {
                        if args.verbose {
                            eprintln!("session: {path}");
                        }
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
            }
        }
    }
}
