//! CLI argument parsing — port of the surface from
//! `packages/coding-agent/src/cli/args.ts` (help text + option set).

use crate::config::VERSION;

/// Upstream `VALID_THINKING_LEVELS` (args.ts).
const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Default)]
pub struct Args {
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    /// --append-system-prompt <text> — appended to the system prompt (repeatable).
    pub append_system_prompt: Vec<String>,
    pub print: bool,
    pub continue_session: bool,
    pub resume: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    /// --fork <path|id> — fork a session by path or partial UUID.
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub no_session: bool,
    pub name: Option<String>,
    pub thinking: Option<String>,
    pub no_tools: bool,
    /// --no-builtin-tools/-nbt — disable built-in tools but keep extension/custom tools.
    pub no_builtin_tools: bool,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    /// --models <list> — comma-separated model catalogs to load.
    pub models: Vec<String>,
    /// --extension/-e <path> — load an extension file (repeatable).
    pub extensions: Vec<String>,
    /// --no-extensions/-ne — disable extension discovery (explicit -e paths still work).
    pub no_extensions: bool,
    /// --skill <path> — load a skill file/dir (repeatable).
    pub skills: Vec<String>,
    /// --no-skills/-ns — disable skills discovery and loading.
    pub no_skills: bool,
    /// --prompt-template <path> — load a prompt-template file/dir (repeatable).
    pub prompt_templates: Vec<String>,
    /// --no-prompt-templates/-np — disable prompt-template discovery.
    pub no_prompt_templates: bool,
    /// --theme <path> — load a theme file/dir (repeatable).
    pub themes: Vec<String>,
    /// --use-theme <name[/name]> — set the initial interactive theme.
    pub use_theme: Option<String>,
    /// --no-themes — disable theme discovery and loading.
    pub no_themes: bool,
    /// --no-context-files/-nc — disable AGENTS.md / CLAUDE.md discovery.
    pub no_context_files: bool,
    pub offline: bool,
    pub verbose: bool,
    pub help: bool,
    pub version: bool,
    pub list_models: Option<String>,
    pub mode: Option<String>,
    pub tui_mode: Option<String>,
    pub export: Option<String>,
    /// --approve/-a: trust project-local files for this run.
    pub approve: bool,
    /// --no-approve/-na: ignore project-local files for this run.
    pub no_approve: bool,
    pub unknown_flags: Vec<String>,
    /// Parse diagnostics, mirroring upstream `Args.diagnostics`.
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

impl Diagnostic {
    fn error(message: impl Into<String>) -> Self {
        Self {
            kind: DiagnosticKind::Error,
            message: message.into(),
        }
    }
    fn warning(message: impl Into<String>) -> Self {
        Self {
            kind: DiagnosticKind::Warning,
            message: message.into(),
        }
    }
}

impl Args {
    /// True when list-models was requested (with optional search pattern).
    pub fn list_models_requested(&self) -> bool {
        self.list_models.is_some()
    }
}

#[allow(clippy::large_enum_variant)] // preserve the public parser outcome shape
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

        // Helpers to fetch a value: inline (`--flag=v`) or the next argv token.
        // Returns None (and records a diagnostic) when no value follows.
        let take_value = |args_state: &mut Args,
                          i: &mut usize,
                          flag: &str,
                          inline: Option<String>|
         -> Option<String> {
            match inline {
                Some(v) => Some(v),
                None => {
                    *i += 1;
                    if *i >= argv.len() {
                        args_state
                            .diagnostics
                            .push(Diagnostic::error(format!("{flag} requires a value")));
                        None
                    } else {
                        Some(argv[*i].clone())
                    }
                }
            }
        };

        // --use-theme: value must start with '-' in which case it's an error
        // (upstream reports "requires a theme name" for a missing/flag-like value).
        if flag == "--use-theme" {
            let v = take_value(&mut args, &mut i, "--use-theme", inline_value);
            match v {
                Some(v) if !v.starts_with('-') => args.use_theme = Some(v),
                _ => args
                    .diagnostics
                    .push(Diagnostic::error("--use-theme requires a theme name")),
            }
            i += 1;
            continue;
        }

        // --tui-mode: regular | fullscreen. Mirrors upstream args.ts token
        // handling: a flag-like or missing value reports
        // "--tui-mode requires ..." without consuming the token; an invalid
        // non-flag value reports the quoted value and is consumed.
        if flag == "--tui-mode" {
            if let Some(inline) = inline_value {
                if inline == "regular" || inline == "fullscreen" {
                    args.tui_mode = Some(inline);
                } else {
                    args.diagnostics.push(Diagnostic::error(format!(
                        "Invalid TUI mode \"{inline}\". Valid values: regular, fullscreen"
                    )));
                }
                i += 1;
                continue;
            }
            match argv.get(i + 1).map(String::as_str) {
                Some("regular") | Some("fullscreen") => {
                    args.tui_mode = argv.get(i + 1).cloned();
                    i += 2; // past --tui-mode and its value
                }
                Some(v) if v.starts_with('-') => {
                    args.diagnostics.push(Diagnostic::error(
                        "--tui-mode requires regular or fullscreen",
                    ));
                    i += 1; // do not consume the flag-like token
                }
                None => {
                    args.diagnostics.push(Diagnostic::error(
                        "--tui-mode requires regular or fullscreen",
                    ));
                    i += 1;
                }
                Some(other) => {
                    args.diagnostics.push(Diagnostic::error(format!(
                        "Invalid TUI mode \"{other}\". Valid values: regular, fullscreen"
                    )));
                    i += 2; // consume the invalid value (upstream i++)
                }
            }
            continue;
        }

        // --thinking: validate against the upstream level set (invalid -> warning).
        if flag == "--thinking" {
            let v = take_value(&mut args, &mut i, "--thinking", inline_value);
            match v {
                Some(v) if VALID_THINKING_LEVELS.contains(&v.as_str()) => args.thinking = Some(v),
                Some(v) => args.diagnostics.push(Diagnostic::warning(format!(
                    "Invalid thinking level \"{v}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                ))),
                None => {}
            }
            i += 1;
            continue;
        }

        // Value-taking flags.
        let value_flags: [&str; 23] = [
            "--provider",
            "--model",
            "--api-key",
            "--system-prompt",
            "--session",
            "--session-id",
            "--fork",
            "--session-dir",
            "--name",
            "-n",
            "--tools",
            "-t",
            "--exclude-tools",
            "-xt",
            "--models",
            "--append-system-prompt",
            "--extension",
            "-e",
            "--skill",
            "--prompt-template",
            "--theme",
            "--mode",
            "--export",
        ];
        if value_flags.contains(&flag.as_str()) {
            let value = match take_value(&mut args, &mut i, flag.as_str(), inline_value) {
                Some(v) => v,
                None => continue,
            };
            match flag.as_str() {
                "--provider" => args.provider = Some(value),
                "--model" => args.model = Some(value),
                "--api-key" => args.api_key = Some(value),
                "--system-prompt" => args.system_prompt = Some(value),
                "--session" => args.session = Some(value),
                "--session-id" => args.session_id = Some(value),
                "--fork" => args.fork = Some(value),
                "--session-dir" => args.session_dir = Some(value),
                "--name" | "-n" => args.name = Some(value),
                "--models" => {
                    args.models = value.split(',').map(|s| s.trim().to_string()).collect();
                }
                "--append-system-prompt" => args.append_system_prompt.push(value),
                "--extension" | "-e" => args.extensions.push(value),
                "--skill" => args.skills.push(value),
                "--prompt-template" => args.prompt_templates.push(value),
                "--theme" => args.themes.push(value),
                "--tools" | "-t" => {
                    args.tools = Some(
                        value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                }
                "--exclude-tools" | "-xt" => {
                    args.exclude_tools = Some(
                        value
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    )
                }
                "--mode" => {
                    if matches!(value.as_str(), "text" | "json" | "rpc") {
                        args.mode = Some(value);
                    } else {
                        args.unknown_flags.push(format!("--mode {value}"));
                    }
                }
                "--export" => args.export = Some(value),
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
                    if i + 1 < argv.len()
                        && !argv[i + 1].starts_with('-')
                        && !argv[i + 1].starts_with('@')
                    {
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
            "--no-builtin-tools" | "-nbt" => args.no_builtin_tools = true,
            "--no-extensions" | "-ne" => args.no_extensions = true,
            "--no-skills" | "-ns" => args.no_skills = true,
            "--no-prompt-templates" | "-np" => args.no_prompt_templates = true,
            "--no-themes" => args.no_themes = true,
            "--no-context-files" | "-nc" => args.no_context_files = true,
            "--approve" | "-a" => args.approve = true,
            "--no-approve" | "-na" => args.no_approve = true,
            "--offline" => args.offline = true,
            "--verbose" => args.verbose = true,
            "--help" | "-h" => return ParseOutcome::Help,
            "--version" | "-v" => return ParseOutcome::Version,
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
    println!(
        "{} - AI coding assistant with read, bash, edit, write tools",
        crate::config::APP_NAME
    );
    println!();
    println!("Usage:");
    println!(
        "  {} [options] [@files...] [messages...]",
        crate::config::APP_NAME
    );
    println!();
    println!("Options:");
    println!("  --provider <name>              Provider name (default: google)");
    println!("  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")");
    println!("  --api-key <key>                API key (defaults to env vars)");
    println!("  --system-prompt <text>         System prompt (default: coding assistant prompt)");
    println!("  --append-system-prompt <text>  Append text or file contents to the system prompt (repeatable)");
    println!("  --print, -p                    Non-interactive mode: process prompt and exit");
    println!("  --continue, -c                 Continue previous session");
    println!("  --resume, -r                   Select a session to resume");
    println!("  --session <path|id>            Use specific session file or partial UUID");
    println!(
        "  --session-id <id>              Use exact project session ID, creating it if missing"
    );
    println!("  --fork <path|id>               Fork a session by file path or partial UUID");
    println!("  --session-dir <dir>            Directory for session storage and lookup");
    println!("  --no-session                   Don't save session (ephemeral)");
    println!("  --name, -n <name>              Set session display name");
    println!("  --models <list>                Comma-separated model catalogs to load");
    println!("  --tools, -t <tools>            Comma-separated allowlist of tool names to enable");
    println!("  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names to disable");
    println!("  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max");
    println!(
        "  --no-tools, -nt                Disable all tools by default (built-in and extension)"
    );
    println!(
        "  --no-builtin-tools, -nbt       Disable built-in tools but keep extension/custom tools"
    );
    println!("  --extension, -e <path>         Load an extension file (repeatable)");
    println!("  --no-extensions, -ne           Disable extension discovery (explicit -e paths still work)");
    println!("  --skill <path>                 Load a skill file or directory (repeatable)");
    println!("  --no-skills, -ns               Disable skills discovery and loading");
    println!(
        "  --prompt-template <path>       Load a prompt template file or directory (repeatable)"
    );
    println!("  --no-prompt-templates, -np     Disable prompt template discovery and loading");
    println!("  --theme <path>                 Load a theme file or directory (repeatable)");
    println!("  --use-theme <name[/name]>      Set the initial interactive theme for this run");
    println!("  --no-themes                    Disable theme discovery and loading");
    println!(
        "  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading"
    );
    println!("  --approve, -a                  Trust project-local files for this run");
    println!("  --no-approve, -na              Ignore project-local files for this run");
    println!("  --offline                      Disable startup network operations (same as PI_OFFLINE=1)");
    println!("  --export <file>                Export session file to HTML and exit");
    println!("  --list-models [search]         List available models (with optional fuzzy search)");
    println!("  --tui-mode <mode>              TUI mode: regular or fullscreen");
    println!(
        "  --verbose                      Force verbose startup (overrides quietStartup setting)"
    );
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
    fn short_v_is_version_not_verbose() {
        // Upstream args.ts maps `-v` to version (there is no short verbose).
        let argv: Vec<String> = vec!["-v".into()];
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
        let args = parse(&[
            "--provider",
            "faux",
            "--model",
            "faux/faux-1",
            "-p",
            "--name",
            "demo",
        ]);
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

    #[test]
    fn parses_no_builtin_tools() {
        let args = parse(&["-nbt"]);
        assert!(args.no_builtin_tools);
        // Long form
        let args = parse(&["--no-builtin-tools"]);
        assert!(args.no_builtin_tools);
    }

    #[test]
    fn parses_fork() {
        let args = parse(&["--fork", "abc123"]);
        assert_eq!(args.fork.as_deref(), Some("abc123"));
        let args = parse(&["--fork=/tmp/session.jsonl"]);
        assert_eq!(args.fork.as_deref(), Some("/tmp/session.jsonl"));
    }

    #[test]
    fn parses_repeatable_extension_skill_theme_prompt_flags() {
        let args = parse(&["-e", "a.ts", "--extension", "b.ts"]);
        assert_eq!(args.extensions, vec!["a.ts", "b.ts"]);
        let args = parse(&["--skill", "s1", "--skill", "s2"]);
        assert_eq!(args.skills, vec!["s1", "s2"]);
        let args = parse(&["--prompt-template", "p1", "--prompt-template", "p2"]);
        assert_eq!(args.prompt_templates, vec!["p1", "p2"]);
        let args = parse(&["--theme", "t1", "--theme", "t2"]);
        assert_eq!(args.themes, vec!["t1", "t2"]);
    }

    #[test]
    fn parses_disable_discovery_flags() {
        let args = parse(&["-ne", "-ns", "-np", "--no-themes", "-nc"]);
        assert!(args.no_extensions);
        assert!(args.no_skills);
        assert!(args.no_prompt_templates);
        assert!(args.no_themes);
        assert!(args.no_context_files);
    }

    #[test]
    fn parses_use_theme_and_validates_missing_value() {
        let args = parse(&["--use-theme", "solarized"]);
        assert_eq!(args.use_theme.as_deref(), Some("solarized"));
        let args = parse(&["--use-theme", "--verbose"]);
        assert!(args.use_theme.is_none());
        assert!(args
            .diagnostics
            .iter()
            .any(|d| d.message.contains("--use-theme")));
    }

    #[test]
    fn parses_append_system_prompt_and_models() {
        let args = parse(&[
            "--append-system-prompt",
            "one",
            "--append-system-prompt",
            "two",
            "--models",
            "anthropic,openai",
        ]);
        assert_eq!(args.append_system_prompt, vec!["one", "two"]);
        assert_eq!(args.models, vec!["anthropic", "openai"]);
    }

    #[test]
    fn thinking_invalid_level_records_diagnostic() {
        let args = parse(&["--thinking", "bogus"]);
        assert!(args.thinking.is_none());
        assert!(args
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Warning
                && d.message.contains("Invalid thinking level")));
    }

    #[test]
    fn tui_mode_parsed_and_invalid_value_diag() {
        let args = parse(&["--tui-mode", "fullscreen"]);
        assert_eq!(args.tui_mode.as_deref(), Some("fullscreen"));
        // Invalid non-flag value: quoted in the message (upstream), consumed.
        let args = parse(&["--tui-mode", "bogus", "--verbose"]);
        assert!(args.tui_mode.is_none());
        assert!(args
            .diagnostics
            .iter()
            .any(|d| d.message == "Invalid TUI mode \"bogus\". Valid values: regular, fullscreen"));
        // The invalid value was consumed; the trailing --verbose parsed.
        assert!(args.verbose);
        // Flag-like value: "--tui-mode requires ..." and the token is NOT
        // consumed (upstream parses it normally).
        let args = parse(&["--tui-mode", "--verbose"]);
        assert!(args.tui_mode.is_none());
        assert!(args
            .diagnostics
            .iter()
            .any(|d| d.message == "--tui-mode requires regular or fullscreen"));
        assert!(args.verbose, "the flag-like token must not be swallowed");
        // Missing value: same diagnostic.
        let args = parse(&["--tui-mode"]);
        assert!(args
            .diagnostics
            .iter()
            .any(|d| d.message == "--tui-mode requires regular or fullscreen"));
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
