//! Slash command registry — port of
//! `packages/coding-agent/src/core/slash-commands.ts`.
//!
//! The command definitions used by interactive mode. This crate owns the
//! registry; the interactive UI that invokes the commands is ported in
//! parallel by another agent.

/// Source of a slash command (upstream `SlashCommandSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandSource {
    Extension,
    Prompt,
    Skill,
}

impl SlashCommandSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SlashCommandSource::Extension => "extension",
            SlashCommandSource::Prompt => "prompt",
            SlashCommandSource::Skill => "skill",
        }
    }
}

/// Information about an available slash command (upstream `SlashCommandInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: String,
}

/// A built-in slash command definition (upstream `BuiltinSlashCommand`).
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

/// The built-in slash command registry. Order and text match upstream exactly.
pub const BUILTIN_SLASH_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand {
        name: "settings",
        description: "Open settings menu",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "model",
        description: "Select model (opens selector UI)",
        argument_hint: Some("<provider/model>"),
    },
    BuiltinSlashCommand {
        name: "thinking",
        description: "Set thinking level",
        argument_hint: Some("<level>"),
    },
    BuiltinSlashCommand {
        name: "scoped-models",
        description: "Enable/disable models for Ctrl+P cycling",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "export",
        description: "Export session (HTML default, or specify path: .html/.jsonl)",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "import",
        description: "Import and resume a session from a JSONL file",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "share",
        description: "Share session as a secret GitHub gist",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "copy",
        description: "Copy last agent message to clipboard",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "name",
        description: "Set session display name",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "session",
        description: "Show session info and stats",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "changelog",
        description: "Show changelog entries",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "hotkeys",
        description: "Show all keyboard shortcuts",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "fork",
        description: "Create a new fork from a previous user message",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "clone",
        description: "Duplicate the current session at the current position",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "tree",
        description: "Navigate session tree (switch branches)",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "trust",
        description: "Save project trust decision for future sessions",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("<provider>"),
    },
    BuiltinSlashCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "compact",
        description: "Manually compact the session context",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "resume",
        description: "Resume a different session",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "reload",
        description: "Reload keybindings, extensions, skills, prompts, themes, and context files",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "quit",
        description: "Quit pi",
        argument_hint: None,
    },
];

/// Look up a built-in command by name.
pub fn find_builtin_command(name: &str) -> Option<&'static BuiltinSlashCommand> {
    BUILTIN_SLASH_COMMANDS.iter().find(|c| c.name == name)
}

/// The commands as `SlashCommandInfo` rows with a synthetic `sourceInfo`
/// (the upstream derives a synthetic source from the running entry point).
pub fn builtin_command_infos(source_info: String) -> Vec<SlashCommandInfo> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|c| SlashCommandInfo {
            name: c.name.to_string(),
            description: Some(c.description.to_string()),
            source: SlashCommandSource::Extension,
            source_info: source_info.clone(),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::APP_NAME;

    #[test]
    fn registry_has_expected_command_count() {
        assert_eq!(BUILTIN_SLASH_COMMANDS.len(), 23);
    }

    #[test]
    fn quit_uses_app_name() {
        let quit = find_builtin_command("quit").unwrap();
        assert_eq!(quit.description, format!("Quit {}", APP_NAME));
    }

    #[test]
    fn argument_hints_match_upstream() {
        assert_eq!(
            find_builtin_command("model").unwrap().argument_hint,
            Some("<provider/model>")
        );
        assert_eq!(
            find_builtin_command("login").unwrap().argument_hint,
            Some("<provider>")
        );
        assert_eq!(
            find_builtin_command("settings").unwrap().argument_hint,
            None
        );
    }

    #[test]
    fn find_unknown_returns_none() {
        assert!(find_builtin_command("does-not-exist").is_none());
    }

    #[test]
    fn infos_have_source_and_description() {
        let infos = builtin_command_infos("test-entry".to_string());
        assert_eq!(infos.len(), 23);
        assert_eq!(infos[0].name, "settings");
        assert_eq!(infos[0].source, SlashCommandSource::Extension);
        assert_eq!(infos[0].source_info, "test-entry");
        assert!(infos[1].description.is_some());
    }
}
