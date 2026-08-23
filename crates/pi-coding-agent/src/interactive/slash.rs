//! Slash commands — port of the command registry subset from
//! `packages/coding-agent/src/core/slash-commands.ts`, plus the interactive
//! loop's dispatch.
//!
//! Each builtin command knows its name, description, argument hint, and
//! whether it opens a modal selector versus acting immediately.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashKind {
    /// Opens the model selector.
    Model,
    /// Opens the thinking-level selector.
    Thinking,
    /// Opens the settings selector.
    Settings,
    /// Opens the theme selector.
    Theme,
    /// Shows session info.
    Session,
    /// Manually compacts the session.
    Compact,
    /// Clears the visible transcript.
    Clear,
    /// Shows the hotkeys/usage help.
    Hotkeys,
    /// Quits the interactive session.
    Quit,
    /// Shows the available commands.
    Help,
    /// Not yet wired (documented divergence): shows a status notice.
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
    pub kind: SlashKind,
}

pub const BUILTIN_SLASH_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand {
        name: "settings",
        description: "Open settings menu",
        argument_hint: None,
        kind: SlashKind::Settings,
    },
    BuiltinSlashCommand {
        name: "model",
        description: "Select model (opens selector UI)",
        argument_hint: Some("<provider/model>"),
        kind: SlashKind::Model,
    },
    BuiltinSlashCommand {
        name: "thinking",
        description: "Set thinking level",
        argument_hint: Some("<level>"),
        kind: SlashKind::Thinking,
    },
    BuiltinSlashCommand {
        name: "theme",
        description: "Select terminal theme",
        argument_hint: None,
        kind: SlashKind::Theme,
    },
    BuiltinSlashCommand {
        name: "session",
        description: "Show session info and stats",
        argument_hint: None,
        kind: SlashKind::Session,
    },
    BuiltinSlashCommand {
        name: "compact",
        description: "Manually compact the session context",
        argument_hint: None,
        kind: SlashKind::Compact,
    },
    BuiltinSlashCommand {
        name: "clear",
        description: "Clear the terminal screen",
        argument_hint: None,
        kind: SlashKind::Clear,
    },
    BuiltinSlashCommand {
        name: "hotkeys",
        description: "Show all keyboard shortcuts",
        argument_hint: None,
        kind: SlashKind::Hotkeys,
    },
    BuiltinSlashCommand {
        name: "help",
        description: "Show available commands",
        argument_hint: None,
        kind: SlashKind::Help,
    },
    BuiltinSlashCommand {
        name: "quit",
        description: "Quit pi",
        argument_hint: None,
        kind: SlashKind::Quit,
    },
    BuiltinSlashCommand {
        name: "export",
        description: "Export session (HTML default, or .html/.jsonl)",
        argument_hint: Some("<path>"),
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "import",
        description: "Import and resume a session from a JSONL file",
        argument_hint: Some("<path>"),
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "share",
        description: "Share session as a secret GitHub gist",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "copy",
        description: "Copy last agent message to clipboard",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "name",
        description: "Set session display name",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "fork",
        description: "Create a new fork from a previous user message",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "clone",
        description: "Duplicate the current session at the current position",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "tree",
        description: "Navigate session tree (switch branches)",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "trust",
        description: "Save project trust decision for future sessions",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("<provider>"),
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "resume",
        description: "Resume a different session",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
    BuiltinSlashCommand {
        name: "reload",
        description: "Reload keybindings and settings",
        argument_hint: None,
        kind: SlashKind::Unsupported,
    },
];

pub fn find_command(name: &str) -> Option<&'static BuiltinSlashCommand> {
    BUILTIN_SLASH_COMMANDS.iter().find(|c| c.name == name)
}

/// True when the text (trimmed) is a full slash command invocation.
pub fn is_slash_invocation(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('/')
}

/// Split an invocation into (command_name, argument).
pub fn parse_invocation(text: &str) -> (Option<&str>, &str) {
    let t = text.trim_start();
    if !t.starts_with('/') {
        return (None, "");
    }
    let rest = &t[1..];
    match rest.find(' ') {
        Some(i) => (Some(&rest[..i]), rest[i + 1..].trim()),
        None => (Some(rest), ""),
    }
}

/// The command completions used by the editor autocomplete.
pub fn command_autocomplete_items() -> Vec<pi_tui::autocomplete::AutocompleteItem> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|c| {
            let desc = match c.argument_hint {
                Some(hint) if !c.description.is_empty() => format!("{hint} — {}", c.description),
                Some(hint) => hint.to_string(),
                None => c.description.to_string(),
            };
            pi_tui::autocomplete::AutocompleteItem {
                value: c.name.to_string(),
                label: c.name.to_string(),
                description: if desc.is_empty() { None } else { Some(desc) },
            }
        })
        .collect()
}
