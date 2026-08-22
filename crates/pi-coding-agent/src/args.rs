//! CLI argument parsing — port of the surface from
//! `packages/coding-agent/src/cli/args.ts` (help text + option set).

use crate::config::VERSION;

#[derive(Debug, Clone, Default)]
pub struct Args {
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub print: bool,
    pub continue_session: bool,
    pub resume: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub session_dir: Option<String>,
    pub no_session: bool,
    pub name: Option<String>,
    pub thinking: Option<String>,
    pub no_tools: bool,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub offline: bool,
    pub verbose: bool,
    pub help: bool,
    pub version: bool,
    pub list_models: Option<String>,
    pub mode: Option<String>,
    pub unknown_flags: Vec<String>,
}

impl Args {
    /// True when list-models was requested (with optional search pattern).
    pub fn list_models_requested(&self) -> bool {
        self.list_models.is_some()
    }
}

pub enum ParseOutcome {
    Run(Args),
    Help,
    Version,
}

/// Parses argv (excluding argv[0]). Flags with values follow clap-style
/// `--flag value` or `--flag=value`; `@file` arguments are collected
/// separately (expansion happens in main).
pub fn parse_args(argv: &[String]) -> ParseOutcome {
    let mut args = Args::default();
    let mut i = 0;
    let mut positional_only = false;
    while i < argv.len() {
        let arg = &argv[i];
        if positional_only || !arg.starts_with('-') || arg == "-" {
            if let Some(rest) = arg.strip_prefix('@') {
                args.file_args.push(rest.to_string());
            } else {
                args.messages.push(arg.clone());
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            i += 1;
            continue;
        }

        // --flag=value form
        let (flag, inline_value) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        // Value-taking flags
        let value_flags: [&str; 17] = [
            "--provider", "--model", "--api-key", "--system-prompt", "--append-system-prompt",
            "--session", "--session-id", "--session-dir", "--name", "-n", "--thinking", "--tools",
            "-t", "--exclude-tools", "-xt", "--tui-mode", "--mode",
        ];
        if value_flags.contains(&flag.as_str()) {
            let value = match inline_value {
                Some(v) => v,
                None => {
                    i += 1;
                    if i >= argv.len() {
                        args.unknown_flags.push(format!("{flag} requires a value"));
                        continue;
                    }
                    argv[i].clone()
                }
            };
            match flag.as_str() {
                "--provider" => args.provider = Some(value),
                "--model" => args.model = Some(value),
                "--api-key" => args.api_key = Some(value),
                "--system-prompt" => args.system_prompt = Some(value),
                "--session" => args.session = Some(value),
                "--session-id" => args.session_id = Some(value),
                "--session-dir" => args.session_dir = Some(value),
                "--name" | "-n" => args.name = Some(value),
                "--thinking" => args.thinking = Some(value),
                "--tools" | "-t" => args.tools = Some(value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
                "--exclude-tools" | "-xt" => args.exclude_tools = Some(value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
                "--mode" => {
                    if matches!(value.as_str(), "text" | "json" | "rpc") {
                        args.mode = Some(value);
                    } else {
                        args.unknown_flags.push(format!("--mode {value}"));
                    }
                }
                _ => {}
            }
            i += 1;
            continue;
        }

        // --list-models [search] — optional search pattern not starting with - or @
        if flag == "--list-models" {
            match inline_value {
                Some(v) => args.list_models = Some(v),
                None => {
                    if i + 1 < argv.len() && !argv[i + 1].starts_with('-') && !argv[i + 1].starts_with('@') {
                        args.list_models = Some(argv[i + 1].clone());
                        i += 1;
                    } else {
                        args.list_models = Some(String::new());
                    }
                }
            }
            i += 1;
            continue;
        }

        // Boolean/short flags
        match flag.as_str() {
            "--print" | "-p" => args.print = true,
            "--continue" | "-c" => args.continue_session = true,
            "--resume" | "-r" => args.resume = true,
            "--no-session" => args.no_session = true,
            "--no-tools" | "-nt" => args.no_tools = true,
            "--offline" => args.offline = true,
            "--verbose" | "-v" => args.verbose = true,
            "--help" | "-h" => return ParseOutcome::Help,
            "--version" => return ParseOutcome::Version,
            _ => args.unknown_flags.push(arg.clone()),
        }
        i += 1;
    }
    // --version/-v anywhere wins over running (upstream prints help when both)
    if args.version {
        return ParseOutcome::Version;
    }
    ParseOutcome::Run(args)
}

pub fn print_version() {
    println!("{} {}", crate::config::APP_NAME, VERSION);
}

pub fn print_help() {
    println!("{} - AI coding assistant with read, bash, edit, write tools", crate::config::APP_NAME);
    println!();
    println!("Usage:");
    println!("  {} [options] [@files...] [messages...]", crate::config::APP_NAME);
    println!();
    println!("Options:");
    println!("  --provider <name>              Provider name (default: google)");
    println!("  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")");
    println!("  --api-key <key>                API key (defaults to env vars)");
    println!("  --system-prompt <text>         System prompt (default: coding assistant prompt)");
    println!("  --print, -p                    Non-interactive mode: process prompt and exit");
    println!("  --continue, -c                 Continue previous session");
    println!("  --resume, -r                   Select a session to resume");
    println!("  --session <path|id>            Use specific session file or partial UUID");
    println!("  --session-id <id>              Use exact project session ID, creating it if missing");
    println!("  --session-dir <dir>            Directory for session storage and lookup");
    println!("  --no-session                   Don't save session (ephemeral)");
    println!("  --name, -n <name>              Set session display name");
    println!("  --tools, -t <tools>            Comma-separated allowlist of tool names to enable");
    println!("  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names to disable");
    println!("  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max");
    println!("  --no-tools, -nt                Disable all tools by default (built-in and extension)");
    println!("  --offline                      Disable startup network operations (same as PI_OFFLINE=1)");
    println!("  --list-models [search]         List available models (with optional fuzzy search)");
    println!("  --verbose                      Force verbose startup (overrides quietStartup setting)");
    println!("  --help, -h                     Show this help");
    println!("  --version, -v                  Show version number");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &[&str]) -> Args {
        let argv: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        match parse_args(&argv) {
            ParseOutcome::Run(a) => a,
            ParseOutcome::Help => panic!("unexpected help"),
            ParseOutcome::Version => panic!("unexpected version"),
        }
    }

    #[test]
    fn version_wins() {
        let argv: Vec<String> = vec!["--version".into()];
        assert!(matches!(parse_args(&argv), ParseOutcome::Version));
    }

    #[test]
    fn help_wins() {
        let argv: Vec<String> = vec!["--help".into()];
        assert!(matches!(parse_args(&argv), ParseOutcome::Help));
    }

    #[test]
    fn parses_positional_messages() {
        let args = parse(&["hello", "world"]);
        assert_eq!(args.messages, vec!["hello", "world"]);
    }

    #[test]
    fn parses_file_args() {
        let args = parse(&["@prompt.md", "what color is the sky?"]);
        assert_eq!(args.file_args, vec!["prompt.md"]);
        assert_eq!(args.messages, vec!["what color is the sky?"]);
    }

    #[test]
    fn parses_flags() {
        let args = parse(&["--provider", "faux", "--model", "faux/faux-1", "-p", "--name", "demo"]);
        assert_eq!(args.provider.as_deref(), Some("faux"));
        assert_eq!(args.model.as_deref(), Some("faux/faux-1"));
        assert!(args.print);
        assert_eq!(args.name.as_deref(), Some("demo"));
    }

    #[test]
    fn parses_equals_form_and_short_flags() {
        let args = parse(&["--provider=faux", "-nt", "-xt=bash,rm", "--no-session"]);
        assert_eq!(args.provider.as_deref(), Some("faux"));
        assert!(args.no_tools);
        assert_eq!(args.exclude_tools.clone().unwrap(), vec!["bash", "rm"]);
        assert!(args.no_session);
    }

    #[test]
    fn double_dash_stops_flag_parsing() {
        let args = parse(&["--", "--provider"]);
        assert_eq!(args.messages, vec!["--provider"]);
        assert!(args.provider.is_none());
    }

    #[test]
    fn records_unknown_flags() {
        let args = parse(&["--not-a-real-flag"]);
        assert_eq!(args.unknown_flags, vec!["--not-a-real-flag"]);
    }
}

impl ParseOutcome {
    /// Test helper: unpack a Run outcome, panicking otherwise.
    pub fn expect_run(self) -> Args {
        match self {
            ParseOutcome::Run(args) => args,
            ParseOutcome::Help => panic!("expected Run outcome, got Help"),
            ParseOutcome::Version => panic!("expected Run outcome, got Version"),
        }
    }
}


#[cfg(test)]
mod mode_parsing {
    use super::*;
    #[test]
    fn parses_mode_rpc() {
        let args = parse_args(&["--mode".to_string(), "rpc".to_string()]).expect_run();
        assert_eq!(args.mode.as_deref(), Some("rpc"));
    }
    #[test]
    fn rejects_invalid_mode() {
        let args = parse_args(&["--mode".to_string(), "wat".to_string()]).expect_run();
        assert!(args.mode.is_none());
        assert!(args.unknown_flags.iter().any(|f| f.contains("wat")));
    }
    #[test]
    fn mode_equals_form() {
        let args = parse_args(&["--mode=rpc".to_string()]).expect_run();
        assert_eq!(args.mode.as_deref(), Some("rpc"));
    }
}
