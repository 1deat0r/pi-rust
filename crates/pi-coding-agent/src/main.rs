//! `pi` binary entry point — port of `packages/coding-agent/src/main.ts`
//! (CLI dispatch; interactive/RPC modes arrive with pi-tui).

use pi_coding_agent::args::{parse_args, print_help, print_version, ParseOutcome};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&argv) {
        ParseOutcome::Help => {
            print_help();
        }
        ParseOutcome::Version => {
            print_version();
        }
        ParseOutcome::Run(args) => {
            if !args.unknown_flags.is_empty() {
                eprintln!("unknown flags: {}", args.unknown_flags.join(", "));
            }
            // --mode rpc: headless JSONL protocol over stdio.
            if args.mode.as_deref() == Some("rpc") {
                let cwd = pi_coding_agent::config::cwd();
                let agent_dir = pi_coding_agent::config::get_agent_dir();
                let settings = pi_coding_agent::core::settings::SettingsManager::create(
                    &cwd,
                    &agent_dir.display().to_string(),
                    pi_coding_agent::core::settings::SettingsManagerCreateOptions::default(),
                );
                if let Err(err) = pi_coding_agent::modes::rpc::run_rpc_mode(&args, settings).await {
                    eprintln!("rpc error: {err}");
                    std::process::exit(1);
                }
                return;
            }
            // --list-models: build the built-in provider registry and print
            // the auth-gated model table (upstream list-models behavior).
            if args.list_models_requested() {
                let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
                let out = pi_coding_agent::list_models::list_models(&models, args.list_models.as_deref());
                print!("{out}");
                return;
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
