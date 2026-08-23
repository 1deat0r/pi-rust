//! Interactive mode engine — port of
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` (the
//! subset backed by the ported TUI components).
//!
//! Owns the transcript + editor scene, slash-command dispatch, selectors
//! (model / thinking / theme / settings), the footer, and the agent turn
//! loop. The terminal event loop lives in `crate::modes::interactive`.

pub mod config_selector;
pub mod footer;
pub mod session_meta;
pub use session_meta::{SessionMetaForPicker, session_picker_items, picker_select_items};
pub mod messages;
pub mod selectors;
pub mod settings_panel;
pub mod slash;
pub mod tui_theme;

use std::sync::{Arc, Mutex};

use pi_agent::types::AgentMessage;
use pi_tui::autocomplete::{CombinedAutocompleteProvider, SlashCommand};
use pi_tui::components::{Editor, EditorOptions, EditorTheme, Markdown, ScrollView, Text};
use pi_tui::keybindings::KeybindingsManager;
use pi_tui::tui::{Scene, SharedComponent};

use crate::core::settings::SettingsManager;
use crate::interactive::footer::FooterData;
use crate::interactive::settings_panel::SettingsPanel;
use crate::interactive::tui_theme as t;

/// A modal overlay taking over input from the editor. Selectors live behind
/// a shared mutex so the frame renderer and input loop share ownership.
pub enum Modal {
    Model(Arc<Mutex<selectors::ListSelector>>),
    Thinking(Arc<Mutex<selectors::ListSelector>>),
    Theme(Arc<Mutex<selectors::ListSelector>>),
    Settings(Arc<Mutex<SettingsPanel>>),
    /// Session picker: selector + the metadata list it was built from.
    Resume(Arc<Mutex<selectors::ListSelector>>, Vec<session_meta::SessionMetaForPicker>),
}

/// Runtime state for the interactive loop.
pub struct InteractiveState {
    pub cwd: String,
    pub settings: SettingsManager,
    pub model_label: String,
    pub provider_label: String,
    pub thinking_level: String,
    pub hide_thinking: bool,
    pub editor: Editor,
    pub transcript_text: String,
    pub footer_data: FooterData,
    pub modal: Option<Modal>,
    pub active_command: Option<&'static slash::BuiltinSlashCommand>,
    pub status_banner: String,
    /// Last submitted text (messages).
    pub messages_shown: usize,
    pub session_entries: usize,
}

/// Create a combined autocomplete provider (slash commands + file paths).
pub fn build_autocomplete_provider(cwd: String) -> CombinedAutocompleteProvider {
    let commands: Vec<SlashCommand> = slash::BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|c| SlashCommand::new(c.name, Some(c.description.to_string()), c.argument_hint.map(|s| s.to_string())))
        .collect();
    let fd_path = std::env::var("PI_FD_PATH").ok().or_else(|| {
        std::process::Command::new("which").arg("fd").output().ok().filter(|o| o.status.success()).map(|o| {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        })
    });
    CombinedAutocompleteProvider::new(commands, cwd, fd_path)
}

/// Create the editor with a keybinding-aware surface.
pub fn create_editor(cwd: String) -> Editor {
    let theme = EditorTheme { border_color: t::editor_border() };
    let options = EditorOptions { padding_x: 1, autocomplete_max_visible: 6 };
    let mut editor = Editor::new(24, theme, options);
    let provider = build_autocomplete_provider(cwd);
    editor.set_autocomplete_provider(Box::new(provider));
    editor
}

/// Set the generated keybinding defaults on the shared manager.
pub fn install_keybindings() {
    let _ = KeybindingsManager::with_defaults(Default::default());
}

/// Render the full scene for the current state.
pub fn build_scene(
    transcript: &Arc<Mutex<Markdown>>,
    editor: &Arc<Mutex<Editor>>,
    footer_component: &Arc<Mutex<Text>>,
    modal_component: Option<SharedComponent>,
    pending: &str,
) -> Arc<Mutex<Scene>> {
    let mut children: Vec<SharedComponent> = Vec::new();
    let scroll_view: SharedComponent = Arc::new(Mutex::new(ScrollView::new(transcript.clone())));
    children.push(scroll_view);
    if let Some(modal) = modal_component {
        children.push(modal);
    }
    children.push(Arc::new(Mutex::new(pi_tui::components::Spacer::new(1))));
    if !pending.is_empty() {
        children.push(Arc::new(Mutex::new(pi_tui::components::Loader::new(pending))));
    }
    children.push(Arc::new(Mutex::new(pi_tui::components::BoxComponent::new(
        editor.clone() as SharedComponent,
        None,
    ))));
    children.push(footer_component.clone());
    Arc::new(Mutex::new(Scene::new(children, None)))
}

/// Handle one submitted input: either a slash command or a prompt.
pub enum SubmitAction {
    Command(&'static slash::BuiltinSlashCommand, Option<String>),
    Prompt(String),
}

/// Parse a submitted line into an action.
pub fn parse_submit(text: &str) -> SubmitAction {
    let trimmed = text.trim();
    if slashtext(trimmed) {
        let (name, argument) = slash::parse_invocation(trimmed);
        if let Some(name) = name {
            if let Some(command) = slash::find_command(name) {
                return SubmitAction::Command(command, (!argument.is_empty()).then(|| argument.to_string()));
            }
        }
        SubmitAction::Prompt(trimmed.to_string())
    } else {
        SubmitAction::Prompt(trimmed.to_string())
    }
}

fn slashtext(text: &str) -> bool {
    slash::is_slash_invocation(text)
}

/// Compose the current transcript markdown from messages + stream buffer.
pub fn compose_transcript(messages: &[AgentMessage], hide_thinking: bool, stream_text: &str) -> String {
    let mut text = messages::build_transcript(messages, hide_thinking);
    if !stream_text.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("▌ ");
        text.push_str(stream_text.trim_start());
    }
    text
}

/// Apply a selected model value ("provider/id") to the state+settings.
/// Returns (provider, id) on success.
pub fn apply_model_selection(
    settings: &mut SettingsManager,
    value: &str,
) -> Option<(String, String)> {
    let (provider, id) = match value.split_once('/') {
        Some((p, i)) => (p.to_string(), i.to_string()),
        None => return None,
    };
    settings.set_default_model_and_provider(provider.clone(), id.clone());
    Some((provider, id))
}

/// Record the editor border color change after model/theme switches.
pub fn editor_border(line: &str) -> String {
    t::fg("editorForeground", line)
}

/// Semver-ish version banner used by /hotkeys and startup.
pub fn version_label() -> String {
    "pi 0.84.2 (port)".to_string()
}

#[cfg(test)]
mod interactive_tests {
    use crate::interactive::{parse_submit, SubmitAction};
    use crate::interactive::slash::{find_command, parse_invocation, BUILTIN_SLASH_COMMANDS};
    use crate::interactive::footer::format_cwd_for_footer;
    use crate::interactive::messages::{format_tokens, build_transcript};

    #[test]
    fn parse_submit_detects_commands() {
        match parse_submit("/model somearg") {
            SubmitAction::Command(cmd, arg) => {
                assert_eq!(cmd.name, "model");
                assert_eq!(arg.as_deref(), Some("somearg"));
            }
            _ => panic!("expected command"),
        }
        match parse_submit("just a prompt") {
            SubmitAction::Prompt(p) => assert_eq!(p, "just a prompt"),
            _ => panic!("expected prompt"),
        }
        match parse_submit("/unknowncmd") {
            SubmitAction::Prompt(p) => assert_eq!(p, "/unknowncmd"),
            _ => panic!("unknown command falls through to prompt"),
        }
    }

    #[test]
    fn slash_registry_covers_builtins() {
        for name in ["settings", "model", "thinking", "theme", "session", "compact", "clear", "hotkeys", "help", "quit"] {
            assert!(find_command(name).is_some(), "missing {name}");
        }
        let names: Vec<&str> = BUILTIN_SLASH_COMMANDS.iter().map(|c| c.name).collect();
        assert!(names.contains(&"hotkeys"));
    }

    #[test]
    fn parse_invocation_splits_name_and_argument() {
        let (name, arg) = parse_invocation("/model anthropic/claude-opus-4-8");
        assert_eq!(name, Some("model"));
        assert_eq!(arg, "anthropic/claude-opus-4-8");
        let (name, arg) = parse_invocation("/quit");
        assert_eq!(name, Some("quit"));
        assert_eq!(arg, "");
    }

    #[test]
    fn footer_formats_home_relative_paths() {
        assert_eq!(format_cwd_for_footer("/home/user", Some("/home/user")), "~");
        assert_eq!(format_cwd_for_footer("/home/user/projects", Some("/home/user")), "~/projects");
        assert_eq!(format_cwd_for_footer("/opt/elsewhere", Some("/home/user")), "/opt/elsewhere");
        assert_eq!(format_cwd_for_footer("/srv", None), "/srv");
    }

    #[test]
    fn token_formatting() {
        assert_eq!(format_tokens(123), "123");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(12_500), "13k");
        assert_eq!(format_tokens(1_600_000), "1.6M");
        assert_eq!(format_tokens(12_000_000), "12M");
    }

    #[test]
    fn transcript_is_empty_without_messages() {
        let text = build_transcript(&[], false);
        assert!(text.is_empty());
    }
}
