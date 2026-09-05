//! CLI argument parsing — port of the surface from
//! `packages/coding-agent/src/cli/args.ts` (help text + option set).

use crate::config::VERSION;

/// Upstream `VALID_THINKING_LEVELS` (args.ts).
pub(crate) const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Default)]
pub struct Args {
    pub messages: Vec<String>,
    /// Content read from non-RPC piped stdin, prepended to the initial prompt.
    pub stdin_content: Option<String>,
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
    /// Parsed values for unknown long flags. The raw list above remains for
    /// compatibility with the auth command and existing callers; this list is
    /// converted to the upstream Map shape by extension startup validation.
    pub extension_flag_values: Vec<(String, ExtensionFlagValue)>,
    /// Parse diagnostics, mirroring upstream `Args.diagnostics`.
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionFlagValue {
    Boolean(bool),
    String(String),
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

/// Parses argv (excluding argv[0]) using the ordered branches from the
/// upstream `parseArgs` implementation. Values are deliberately consumed
/// exactly as upstream does: known value flags consume the following token
/// even when it looks like another flag, while unknown long flags consume only
/// a following non-flag/non-`@` token. `@file` arguments are collected
/// separately (expansion happens in main).
/// Parse argv while retaining the complete `Args` value, including options
/// needed by the help/runtime bootstrap before the final outcome is chosen.
pub fn parse_args_raw(argv: &[String]) -> Args {
    let mut args = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with('-') {
            if let Some(rest) = arg.strip_prefix('@') {
                args.file_args.push(rest.to_string());
            } else {
                args.messages.push(arg.clone());
            }
            i += 1;
            continue;
        }

        // Exact flag matching is intentional. The pinned oracle does not
        // treat `--flag=value` as a known option; such spellings fall through
        // to unknown long-flag handling below.
        let flag = arg.as_str();

        // --use-theme: value must start with '-' in which case it's an error
        // (upstream reports "requires a theme name" for a missing/flag-like value).
        if flag == "--use-theme" {
            match argv.get(i + 1).map(String::as_str) {
                Some(value) if !value.starts_with('-') => {
                    args.use_theme = Some(value.to_string());
                    i += 2;
                }
                Some(_) | None => {
                    args.diagnostics
                        .push(Diagnostic::error("--use-theme requires a theme name"));
                    // Do not swallow a flag-like token; it is parsed next.
                    i += 1;
                }
            }
            continue;
        }

        // --tui-mode: regular | fullscreen. A flag-like or missing value
        // reports an error without consuming the token; an invalid non-flag
        // value reports the quoted value and is consumed.
        if flag == "--tui-mode" {
            match argv.get(i + 1).map(String::as_str) {
                Some("regular") | Some("fullscreen") => {
                    args.tui_mode = argv.get(i + 1).cloned();
                    i += 2;
                }
                Some(value) if value.starts_with('-') => {
                    args.diagnostics.push(Diagnostic::error(
                        "--tui-mode requires regular or fullscreen",
                    ));
                    i += 1;
                }
                None => {
                    args.diagnostics.push(Diagnostic::error(
                        "--tui-mode requires regular or fullscreen",
                    ));
                    i += 1;
                }
                Some(value) => {
                    args.diagnostics.push(Diagnostic::error(format!(
                        "Invalid TUI mode \"{value}\". Valid values: regular, fullscreen"
                    )));
                    i += 2;
                }
            }
            continue;
        }

        // --thinking: validate against the upstream level set (invalid -> warning).
        if flag == "--thinking" {
            if let Some(value) = argv.get(i + 1).cloned() {
                if VALID_THINKING_LEVELS.contains(&value.as_str()) {
                    args.thinking = Some(value);
                } else {
                    args.diagnostics.push(Diagnostic::warning(format!(
                        "Invalid thinking level \"{value}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                    )));
                }
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // Value-taking flags. Except for --name/-n, a missing value is
        // silently ignored by the upstream parser.
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
        if value_flags.contains(&flag) {
            let Some(value) = argv.get(i + 1).cloned() else {
                if flag == "--name" || flag == "-n" {
                    args.diagnostics
                        .push(Diagnostic::error("--name requires a value"));
                }
                i += 1;
                continue;
            };
            match flag {
                "--provider" => args.provider = Some(value),
                "--model" => args.model = Some(value),
                "--api-key" => args.api_key = Some(value),
                "--system-prompt" => args.system_prompt = Some(value),
                "--session" => args.session = Some(value),
                "--session-id" => args.session_id = Some(value),
                "--fork" => args.fork = Some(value),
                "--session-dir" => args.session_dir = Some(value),
                "--name" | "-n" => {
                    if value.trim().is_empty() {
                        // The upstream main validates normalizeSessionName
                        // immediately after parsing. Emit the same
                        // process-facing diagnostic here because this Rust
                        // entry point receives only ParseOutcome.
                        args.diagnostics
                            .push(Diagnostic::error("--name requires a non-empty value"));
                    } else {
                        args.name = Some(value);
                    }
                }
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
                    }
                }
                "--export" => args.export = Some(value),
                _ => {}
            }
            // Known value flags consume the next token even if it starts with
            // `-`, matching args.ts.
            i += 2;
            continue;
        }

        // --list-models [search] — optional search pattern not starting with - or @
        if flag == "--list-models" {
            if i + 1 < argv.len() && !argv[i + 1].starts_with('-') && !argv[i + 1].starts_with('@')
            {
                args.list_models = Some(argv[i + 1].clone());
                i += 2;
            } else {
                args.list_models = Some(String::new());
                i += 1;
            }
            continue;
        }

        // --print has one intentional positional special case: a following
        // `---literal` is a message, while ordinary flag-like tokens remain
        // available for parsing on the next iteration.
        if flag == "--print" || flag == "-p" {
            args.print = true;
            if let Some(next) = argv.get(i + 1) {
                if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---")) {
                    args.messages.push(next.clone());
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        // Boolean/short flags.
        match flag {
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
            // The upstream parser stores one optional override, so when both
            // spellings are present the later argv token wins. Keep the
            // public boolean fields for compatibility while clearing the
            // superseded side here.
            "--approve" | "-a" => {
                args.approve = true;
                args.no_approve = false;
            }
            "--no-approve" | "-na" => {
                args.no_approve = true;
                args.approve = false;
            }
            "--offline" => args.offline = true,
            "--verbose" => args.verbose = true,
            "--help" | "-h" => args.help = true,
            "--version" | "-v" => args.version = true,
            _ if flag.starts_with("--") => {
                // Unknown long flags are extension-facing. Preserve the raw
                // spelling for the existing Rust dispatch surface and consume
                // an optional non-flag/non-`@` value as upstream does.
                args.unknown_flags.push(arg.clone());
                // An inline `--flag=value` already has its value. The oracle
                // does not consume the following positional token in that
                // case; only a bare unknown long flag may consume one.
                if !arg.contains('=') {
                    if let Some(next) = argv.get(i + 1) {
                        if !next.starts_with('-') && !next.starts_with('@') {
                            args.extension_flag_values.push((
                                arg[2..].to_string(),
                                ExtensionFlagValue::String(next.clone()),
                            ));
                            i += 1;
                        } else {
                            args.extension_flag_values
                                .push((arg[2..].to_string(), ExtensionFlagValue::Boolean(true)));
                        }
                    } else {
                        args.extension_flag_values
                            .push((arg[2..].to_string(), ExtensionFlagValue::Boolean(true)));
                    }
                } else if let Some((name, value)) = arg[2..].split_once('=') {
                    args.extension_flag_values.push((
                        name.to_string(),
                        ExtensionFlagValue::String(value.to_string()),
                    ));
                }
            }
            _ if flag.starts_with('-') => {
                args.diagnostics
                    .push(Diagnostic::error(format!("Unknown option: {flag}")));
            }
            _ => args.messages.push(arg.clone()),
        }
        i += 1;
    }
    args
}

pub fn parse_args(argv: &[String]) -> ParseOutcome {
    // Upstream retains both switches while parsing, then handles version
    // before help. This makes `pi --help --version` deterministic regardless
    // of argument order.
    let args = parse_args_raw(argv);
    if args.version {
        return ParseOutcome::Version;
    }
    if args.help {
        return ParseOutcome::Help;
    }
    ParseOutcome::Run(args)
}

pub fn print_version() {
    println!("{} {}", crate::config::APP_NAME, VERSION);
}

pub fn print_help() {
    print!("{}", include_str!("help.txt"));
}

/// Print the base CLI help plus flags registered by the active native
/// extension set.  The extension block is formatted byte-for-byte like the
/// pinned upstream `printHelp(extensionFlags)` implementation; callers that
/// do not have an extension runtime can use [`print_help`] for the exact base
/// resource.
pub fn print_help_with_extension_flags(
    extension_flags: &[crate::core::extensions::types::ExtensionFlag],
) {
    print!("{}", render_help_with_extension_flags(extension_flags));
}

/// Return the exact help text, including the final newline emitted by the
/// upstream console logger, with an optional native-extension flag section.
pub fn render_help_with_extension_flags(
    extension_flags: &[crate::core::extensions::types::ExtensionFlag],
) -> String {
    if extension_flags.is_empty() {
        return format!("{}\n", include_str!("help.txt"));
    }

    let mut help = include_str!("help.txt").to_string();
    let marker =
        "Extensions can register additional flags (e.g., --plan from plan-mode extension).\n";
    let Some(marker_end) = help.find(marker).map(|index| index + marker.len()) else {
        return format!("{help}\n");
    };

    let mut block = String::from("\nExtension CLI Flags:\n");
    for flag in extension_flags {
        let value = matches!(
            flag.flag_type,
            crate::core::extensions::types::FlagType::String
        )
        .then_some(" <value>")
        .unwrap_or_default();
        let description = flag
            .description
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Registered by {}", flag.extension_path));
        block.push_str(&format!("  --{}{value}", flag.name));
        let padding = 30usize.saturating_sub(format!("  --{}{value}", flag.name).len());
        block.push_str(&" ".repeat(padding));
        block.push_str(&description);
        block.push('\n');
    }
    help.insert_str(marker_end, &block);
    help.push('\n');
    help
}

#[allow(dead_code)]
fn print_help_legacy() {
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
    println!(
        "  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")"
    );
    println!("  --api-key <key>                API key (defaults to env vars)");
    println!("  --system-prompt <text>         System prompt (default: coding assistant prompt)");
    println!(
        "  --append-system-prompt <text>  Append text or file contents to the system prompt (repeatable)"
    );
    println!("  --mode <mode>                  Output mode: text (default), json, or rpc");
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
    println!(
        "  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max"
    );
    println!(
        "  --no-tools, -nt                Disable all tools by default (built-in and extension)"
    );
    println!(
        "  --no-builtin-tools, -nbt       Disable built-in tools but keep extension/custom tools"
    );
    println!("  --extension, -e <path>         Load an extension file (repeatable)");
    println!(
        "  --no-extensions, -ne           Disable extension discovery (explicit -e paths still work)"
    );
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
    println!(
        "  --offline                      Disable startup network operations (same as PI_OFFLINE=1)"
    );
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn version_wins_over_help_in_either_argument_order() {
        for argv in [
            vec!["--help".to_string(), "--version".to_string()],
            vec!["--version".to_string(), "--help".to_string()],
        ] {
            assert!(matches!(parse_args(&argv), ParseOutcome::Version));
        }
    }

    #[test]
    fn parses_positional_messages() {
        let args = parse(&["hello", "world"]);
        assert_eq!(args.messages, vec!["hello", "world"]);
    }
    #[test]
    fn positional_messages_keep_unicode_and_whitespace_verbatim() {
        let args = parse(&["héllo wörld 🌍", "  padded  ", "\ttabbed", "   "]);
        assert_eq!(
            args.messages,
            vec!["héllo wörld 🌍", "  padded  ", "\ttabbed", "   "]
        );
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
    fn parses_value_forms_and_short_flags() {
        let args = parse(&[
            "--provider",
            "faux",
            "-nt",
            "-xt",
            "bash,rm",
            "--no-session",
        ]);
        assert_eq!(args.provider.as_deref(), Some("faux"));
        assert!(args.no_tools);
        assert_eq!(args.exclude_tools.clone().unwrap(), vec!["bash", "rm"]);
        assert!(args.no_session);
    }

    #[test]
    fn project_trust_override_follows_last_argv_flag() {
        let approve_last = parse(&["--no-approve", "--approve"]);
        assert!(approve_last.approve);
        assert!(!approve_last.no_approve);

        let deny_last = parse(&["--approve", "--no-approve"]);
        assert!(!deny_last.approve);
        assert!(deny_last.no_approve);
    }

    #[test]
    fn equals_forms_are_unknown_like_the_upstream_parser() {
        let args = parse(&["--provider=faux", "--mode=rpc", "-xt=bash,rm"]);
        assert!(args.provider.is_none());
        assert!(args.mode.is_none());
        assert_eq!(args.unknown_flags, vec!["--provider=faux", "--mode=rpc"]);
        assert_eq!(
            args.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unknown option: -xt=bash,rm"]
        );

        // Inline values belong to the unknown flag itself; the following
        // positional token remains a message instead of being swallowed as a
        // second unknown-flag value.
        let args = parse(&["--provider=faux", "tail"]);
        assert!(args.provider.is_none());
        assert_eq!(args.unknown_flags, vec!["--provider=faux"]);
        assert_eq!(args.messages, vec!["tail"]);
    }

    #[test]
    fn double_dash_is_an_unknown_long_flag_like_the_upstream_parser() {
        let args = parse(&["--", "--provider"]);
        assert_eq!(args.unknown_flags, vec!["--"]);
        assert!(args.provider.is_none());
    }

    #[test]
    fn every_value_flag_requires_a_separate_token_for_an_inline_value() {
        let argv = [
            "--provider=faux",
            "--model=faux-1",
            "--api-key=secret",
            "--system-prompt=system",
            "--append-system-prompt=append",
            "--session=path",
            "--session-id=id",
            "--fork=source",
            "--session-dir=/tmp/sessions",
            "--name=name",
            "-n=name",
            "--tools=read,bash",
            "-t=read,bash",
            "--exclude-tools=bash",
            "-xt=bash",
            "--models=faux-1",
            "--extension=extension.ts",
            "-e=extension.ts",
            "--skill=skill.md",
            "--prompt-template=prompt.md",
            "--theme=theme.json",
            "--thinking=high",
            "--mode=json",
            "--export=export.html",
            "--use-theme=solarized",
            "--tui-mode=fullscreen",
            "--list-models=faux",
            "--print=true",
        ];
        let args = parse(&argv);
        assert!(args.provider.is_none());
        assert!(args.model.is_none());
        assert!(args.api_key.is_none());
        assert!(args.system_prompt.is_none());
        assert!(args.append_system_prompt.is_empty());
        assert!(args.session.is_none());
        assert!(args.session_id.is_none());
        assert!(args.fork.is_none());
        assert!(args.session_dir.is_none());
        assert!(args.name.is_none());
        assert!(args.tools.is_none());
        assert!(args.exclude_tools.is_none());
        assert!(args.models.is_empty());
        assert!(args.extensions.is_empty());
        assert!(args.skills.is_empty());
        assert!(args.prompt_templates.is_empty());
        assert!(args.themes.is_empty());
        assert!(args.thinking.is_none());
        assert!(args.mode.is_none());
        assert!(args.export.is_none());
        assert_eq!(
            args.unknown_flags,
            argv.iter()
                .filter(|arg| arg.starts_with("--") && arg.contains('='))
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            args.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            [
                "Unknown option: -n=name",
                "Unknown option: -t=read,bash",
                "Unknown option: -xt=bash",
                "Unknown option: -e=extension.ts"
            ]
        );
    }

    #[test]
    fn known_value_flags_consume_flag_like_values() {
        let args = parse(&[
            "--provider",
            "--not-a-provider",
            "--model",
            "-not-a-model",
            "--api-key",
            "@not-a-file",
            "--system-prompt",
            "-system",
            "--append-system-prompt",
            "--append",
            "--session",
            "--session-path",
            "--session-id",
            "--id",
            "--fork",
            "--source",
            "--session-dir",
            "--sessions",
            "--name",
            "--name-value",
            "--tools",
            "--tool",
            "--exclude-tools",
            "--deny",
            "--models",
            "--catalog",
            "--extension",
            "--extension-path",
            "--skill",
            "--skill-path",
            "--prompt-template",
            "--template",
            "--theme",
            "--theme-path",
            "--mode",
            "--invalid-mode",
            "--export",
            "--output",
        ]);
        assert_eq!(args.provider.as_deref(), Some("--not-a-provider"));
        assert_eq!(args.model.as_deref(), Some("-not-a-model"));
        assert_eq!(args.api_key.as_deref(), Some("@not-a-file"));
        assert_eq!(args.system_prompt.as_deref(), Some("-system"));
        assert_eq!(args.append_system_prompt, ["--append"]);
        assert_eq!(args.session.as_deref(), Some("--session-path"));
        assert_eq!(args.session_id.as_deref(), Some("--id"));
        assert_eq!(args.fork.as_deref(), Some("--source"));
        assert_eq!(args.session_dir.as_deref(), Some("--sessions"));
        assert_eq!(args.name.as_deref(), Some("--name-value"));
        assert_eq!(
            args.tools.as_deref(),
            Some(["--tool".to_string()].as_slice())
        );
        assert_eq!(
            args.exclude_tools.as_deref(),
            Some(["--deny".to_string()].as_slice())
        );
        assert_eq!(args.models, ["--catalog"]);
        assert_eq!(args.extensions, ["--extension-path"]);
        assert_eq!(args.skills, ["--skill-path"]);
        assert_eq!(args.prompt_templates, ["--template"]);
        assert_eq!(args.themes, ["--theme-path"]);
        assert_eq!(args.export.as_deref(), Some("--output"));
        // Invalid --mode is consumed and ignored, with no diagnostic.
        assert!(args.mode.is_none());
        assert!(args.unknown_flags.is_empty());
        assert!(args.diagnostics.is_empty());

        let args = parse(&["-xt", "--deny"]);
        assert_eq!(
            args.exclude_tools.as_deref(),
            Some(["--deny".to_string()].as_slice())
        );
        assert!(args.unknown_flags.is_empty());
        assert!(args.diagnostics.is_empty());

        let args = parse(&["--thinking", "--thinking-level"]);
        assert!(args.thinking.is_none());
        assert_eq!(args.diagnostics.len(), 1);
        assert!(args.diagnostics[0]
            .message
            .contains("Invalid thinking level"));
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
        assert!(args.fork.is_none());
        assert_eq!(args.unknown_flags, vec!["--fork=/tmp/session.jsonl"]);
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
    fn missing_optional_value_is_ignored_except_name() {
        for flag in [
            "--provider",
            "--model",
            "--api-key",
            "--system-prompt",
            "--append-system-prompt",
            "--session",
            "--session-id",
            "--fork",
            "--session-dir",
            "--tools",
            "-t",
            "--exclude-tools",
            "-xt",
            "--models",
            "--extension",
            "-e",
            "--skill",
            "--prompt-template",
            "--theme",
            "--thinking",
            "--mode",
            "--export",
        ] {
            let args = parse(&[flag]);
            assert!(
                args.diagnostics.is_empty(),
                "{flag} should be silent at EOF"
            );
            assert!(args.unknown_flags.is_empty(), "{flag} should be recognized");
        }

        for flag in ["--name", "-n"] {
            let args = parse(&[flag]);
            assert_eq!(
                args.diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>(),
                ["--name requires a value"],
                "{flag} has the upstream required-value diagnostic"
            );
        }

        let args = parse(&["--list-models"]);
        assert_eq!(args.list_models, Some(String::new()));
        assert!(args.diagnostics.is_empty());

        let args = parse(&["--print"]);
        assert!(args.print);
        assert!(args.diagnostics.is_empty());
    }

    #[test]
    fn empty_session_name_has_the_main_process_diagnostic() {
        for value in ["", "   "] {
            let args = parse(&["--name", value]);
            assert!(args.name.is_none());
            assert_eq!(
                args.diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>(),
                ["--name requires a non-empty value"]
            );
        }
    }

    #[test]
    fn print_consumes_triple_dash_message_but_not_flags_or_files() {
        let args = parse(&["--print", "---literal"]);
        assert!(args.print);
        assert_eq!(args.messages, vec!["---literal"]);

        let args = parse(&["--print", "--verbose"]);
        assert!(args.print);
        assert!(args.verbose);
        assert!(args.messages.is_empty());

        let args = parse(&["--print", "@prompt.md"]);
        assert!(args.print);
        assert_eq!(args.file_args, vec!["prompt.md"]);
        assert!(args.messages.is_empty());
    }

    #[test]
    fn unknown_long_value_is_consumed_and_unknown_short_is_a_diagnostic() {
        let args = parse(&["--extension-flag", "value", "tail"]);
        assert_eq!(args.unknown_flags, vec!["--extension-flag"]);
        assert_eq!(
            args.extension_flag_values,
            vec![(
                "extension-flag".to_string(),
                ExtensionFlagValue::String("value".to_string())
            )]
        );
        assert_eq!(args.messages, vec!["tail"]);

        let args = parse(&["-x", "tail"]);
        assert!(args.unknown_flags.is_empty());
        assert_eq!(args.messages, vec!["tail"]);
        assert_eq!(
            args.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unknown option: -x"]
        );

        let args = parse(&["--boolean", "--string=value", "--bare"]);
        assert_eq!(
            args.extension_flag_values,
            vec![
                ("boolean".to_string(), ExtensionFlagValue::Boolean(true)),
                (
                    "string".to_string(),
                    ExtensionFlagValue::String("value".to_string())
                ),
                ("bare".to_string(), ExtensionFlagValue::Boolean(true)),
            ]
        );
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

    #[test]
    fn standalone_dash_is_an_unknown_option() {
        let args = parse(&["-"]);
        assert!(args.messages.is_empty());
        assert!(args.unknown_flags.is_empty());
        assert_eq!(
            args.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["Unknown option: -"]
        );
    }

    #[test]
    fn help_resource_contains_the_complete_pinned_surface() {
        let help = include_str!("help.txt");
        for section in [
            "Commands:",
            "Options:",
            "Extensions can register additional flags",
            "Examples:",
            "Environment Variables:",
            "Built-in Tool Names:",
        ] {
            assert!(
                help.contains(section),
                "help is missing section {section:?}"
            );
        }
        for flag in [
            "--no-builtin-tools, -nbt",
            "--no-context-files, -nc",
            "--list-models [search]",
            "--tui-mode <mode>",
        ] {
            assert!(help.contains(flag), "help is missing flag {flag}");
        }
    }

    #[test]
    fn extension_help_uses_upstream_flag_block_and_final_newline() {
        use crate::core::extensions::types::{ExtensionFlag, FlagType};

        let flags = vec![
            ExtensionFlag {
                name: "plan".to_string(),
                description: Some("Enable plan mode".to_string()),
                flag_type: FlagType::Boolean,
                default: Some(serde_json::Value::Bool(false)),
                extension_path: "native://plan".to_string(),
            },
            ExtensionFlag {
                name: "profile".to_string(),
                description: None,
                flag_type: FlagType::String,
                default: None,
                extension_path: "native://profile".to_string(),
            },
        ];
        let rendered = render_help_with_extension_flags(&flags);

        assert!(rendered.contains("\nExtension CLI Flags:\n"));
        assert!(rendered.contains("  --plan                      Enable plan mode\n"));
        assert!(rendered.contains("  --profile <value>           Registered by native://profile\n"));
        assert!(rendered.ends_with("\n\n"));
        assert_eq!(
            render_help_with_extension_flags(&[]),
            format!("{}\n", include_str!("help.txt"))
        );
    }
}

impl ParseOutcome {
    /// Test helper: unpack a Run outcome, panicking otherwise.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
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
        assert!(args.unknown_flags.is_empty());
        assert!(args.diagnostics.is_empty());
    }
    #[test]
    fn mode_equals_form() {
        let args = parse_args(&["--mode=rpc".to_string()]).expect_run();
        assert!(args.mode.is_none());
        assert_eq!(args.unknown_flags, vec!["--mode=rpc"]);
    }
}
