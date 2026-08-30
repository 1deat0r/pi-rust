//! Slash commands — port of the command registry subset from
//! `packages/coding-agent/src/core/slash-commands.ts`, plus the interactive
//! loop's dispatch.
//!
//! Each builtin command knows its name, description, argument hint, and
//! whether it opens a modal selector versus acting immediately.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashKind {
    /// Opens the model selector.
    Model,
    /// Opens the thinking-level selector.
    Thinking,
    /// Opens the scoped-model cycling selector.
    ScopedModels,
    /// Opens the settings selector.
    Settings,
    /// Opens the theme selector.
    Theme,
    /// Shows session info.
    Session,
    /// Shows the shipped changelog catalogue.
    Changelog,
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
    /// Starts provider authentication.
    Login,
    /// Removes a stored provider credential.
    Logout,
    /// Writes a debug snapshot to the agent directory.
    Debug,
    /// Shows the Armin hidden component.
    ArminSaysHi,
    /// Shows the Earendil announcement hidden component.
    DementedDelves,
    /// Export the current session.
    Export,
    /// Import and resume a session.
    Import,
    /// Share the current session.
    Share,
    /// Copy the last assistant message.
    Copy,
    /// Set the session display name.
    Name,
    /// Fork from a previous user message.
    Fork,
    /// Clone the current session.
    Clone,
    /// Show the session tree.
    Tree,
    /// Set the project trust default.
    Trust,
    /// Start a new session.
    New,
    /// Resume a different session.
    Resume,
    /// Reload settings and extensions.
    Reload,
    /// Open the native llama.cpp model manager.
    Llama,
}

#[derive(Debug, Clone)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
    pub kind: SlashKind,
}

/// The public builtin registry is intentionally the exact 0.84.2 list.
/// Native/experimental handlers may remain in `SlashKind`, but commands that
/// are not in this list must fall through to extension/prompt dispatch.
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
        name: "scoped-models",
        description: "Enable/disable models for Ctrl+P cycling",
        argument_hint: None,
        kind: SlashKind::ScopedModels,
    },
    BuiltinSlashCommand {
        name: "export",
        description: "Export session (HTML default, or specify path: .html/.jsonl)",
        argument_hint: None,
        kind: SlashKind::Export,
    },
    BuiltinSlashCommand {
        name: "import",
        description: "Import and resume a session from a JSONL file",
        argument_hint: None,
        kind: SlashKind::Import,
    },
    BuiltinSlashCommand {
        name: "share",
        description: "Share session as a secret GitHub gist",
        argument_hint: None,
        kind: SlashKind::Share,
    },
    BuiltinSlashCommand {
        name: "copy",
        description: "Copy last agent message to clipboard",
        argument_hint: None,
        kind: SlashKind::Copy,
    },
    BuiltinSlashCommand {
        name: "name",
        description: "Set session display name",
        argument_hint: None,
        kind: SlashKind::Name,
    },
    BuiltinSlashCommand {
        name: "session",
        description: "Show session info and stats",
        argument_hint: None,
        kind: SlashKind::Session,
    },
    BuiltinSlashCommand {
        name: "changelog",
        description: "Show changelog entries",
        argument_hint: None,
        kind: SlashKind::Changelog,
    },
    BuiltinSlashCommand {
        name: "hotkeys",
        description: "Show all keyboard shortcuts",
        argument_hint: None,
        kind: SlashKind::Hotkeys,
    },
    BuiltinSlashCommand {
        name: "fork",
        description: "Create a new fork from a previous user message",
        argument_hint: None,
        kind: SlashKind::Fork,
    },
    BuiltinSlashCommand {
        name: "clone",
        description: "Duplicate the current session at the current position",
        argument_hint: None,
        kind: SlashKind::Clone,
    },
    BuiltinSlashCommand {
        name: "tree",
        description: "Navigate session tree (switch branches)",
        argument_hint: None,
        kind: SlashKind::Tree,
    },
    BuiltinSlashCommand {
        name: "trust",
        description: "Save project trust decision for future sessions",
        argument_hint: None,
        kind: SlashKind::Trust,
    },
    BuiltinSlashCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("<provider>"),
        kind: SlashKind::Login,
    },
    BuiltinSlashCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: None,
        kind: SlashKind::Logout,
    },
    BuiltinSlashCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
        kind: SlashKind::New,
    },
    BuiltinSlashCommand {
        name: "compact",
        description: "Manually compact the session context",
        argument_hint: None,
        kind: SlashKind::Compact,
    },
    BuiltinSlashCommand {
        name: "resume",
        description: "Resume a different session",
        argument_hint: None,
        kind: SlashKind::Resume,
    },
    BuiltinSlashCommand {
        name: "reload",
        description: "Reload keybindings, extensions, skills, prompts, themes, and context files",
        argument_hint: None,
        kind: SlashKind::Reload,
    },
    BuiltinSlashCommand {
        name: "quit",
        description: "Quit pi",
        argument_hint: None,
        kind: SlashKind::Quit,
    },
];

pub fn find_command(name: &str) -> Option<&'static BuiltinSlashCommand> {
    BUILTIN_SLASH_COMMANDS.iter().find(|c| c.name == name)
}

const HIDDEN_SLASH_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand {
        name: "debug",
        description: "Write a debug render/session snapshot",
        argument_hint: None,
        kind: SlashKind::Debug,
    },
    BuiltinSlashCommand {
        name: "arminsayshi",
        description: "Show the Armin greeting",
        argument_hint: None,
        kind: SlashKind::ArminSaysHi,
    },
    BuiltinSlashCommand {
        name: "dementedelves",
        description: "Show the Earendil announcement",
        argument_hint: None,
        kind: SlashKind::DementedDelves,
    },
];

/// Hidden commands are executable but intentionally absent from the public
/// registry, autocomplete, and help, matching upstream behavior.
pub fn find_hidden_command(name: &str) -> Option<&'static BuiltinSlashCommand> {
    HIDDEN_SLASH_COMMANDS
        .iter()
        .find(|command| command.name == name)
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

/// Render the built-in command summary shown by `/help`.
pub fn help_banner() -> String {
    let names = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| format!("/{}", command.name))
        .collect::<Vec<_>>()
        .join(" ");
    format!("commands: {names}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_upstream_scoped_and_changelog_commands() {
        assert_eq!(BUILTIN_SLASH_COMMANDS.len(), 23);
        let names: Vec<_> = BUILTIN_SLASH_COMMANDS
            .iter()
            .map(|command| command.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "settings",
                "model",
                "thinking",
                "scoped-models",
                "export",
                "import",
                "share",
                "copy",
                "name",
                "session",
                "changelog",
                "hotkeys",
                "fork",
                "clone",
                "tree",
                "trust",
                "login",
                "logout",
                "new",
                "compact",
                "resume",
                "reload",
                "quit",
            ]
        );
        assert_eq!(
            find_command("scoped-models").map(|command| &command.kind),
            Some(&SlashKind::ScopedModels)
        );
        assert_eq!(
            find_command("changelog").map(|command| &command.kind),
            Some(&SlashKind::Changelog)
        );
        for removed in ["help", "theme", "clear", "llama"] {
            assert!(
                find_command(removed).is_none(),
                "unexpected public builtin {removed}"
            );
        }
    }

    #[test]
    fn help_and_invocation_cover_new_commands() {
        let banner = help_banner();
        assert!(banner.contains("/scoped-models"));
        assert!(banner.contains("/changelog"));
        assert_eq!(
            parse_invocation("  /scoped-models"),
            (Some("scoped-models"), "")
        );
        assert_eq!(parse_invocation("/changelog"), (Some("changelog"), ""));
    }
}
