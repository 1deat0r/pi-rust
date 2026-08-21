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
