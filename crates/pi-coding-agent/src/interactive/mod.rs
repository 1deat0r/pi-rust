//! Interactive mode engine — port of
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` (the
//! subset backed by the ported TUI components).
//!
//! Owns the transcript + editor scene, slash-command dispatch, selectors
//! (model / thinking / theme / settings), the footer, and the agent turn
//! loop. The terminal event loop lives in `crate::modes::interactive`.

pub mod auth;
pub mod clipboard;
pub mod config_selector;
pub mod easter_eggs;
pub mod external_editor;
pub mod footer;
pub mod llama;
pub mod mermaid;
pub mod session_meta;
pub use session_meta::{picker_select_items, session_picker_items, SessionMetaForPicker};
pub mod messages;
pub mod selectors;
pub mod settings_panel;
pub mod slash;
pub mod startup;
pub mod tree_selector;
pub mod tui_theme;

use std::sync::{Arc, Mutex};

use pi_agent::types::AgentMessage;
use pi_tui::autocomplete::{CombinedAutocompleteProvider, SlashCommand};
use pi_tui::components::{
    Editor, EditorOptions, EditorTheme, Markdown, ScrollView, Spacer, Text, VStack,
};
use pi_tui::keybindings::KeybindingsManager;
use pi_tui::tui::{Container, Scene, SharedComponent};
use pi_tui::{LayoutBasis, StackLayoutEntry};

use crate::core::settings::SettingsManager;
use crate::interactive::footer::FooterData;
use crate::interactive::settings_panel::SettingsPanel;
use crate::interactive::tui_theme as t;

/// A modal overlay taking over input from the editor. Selectors live behind
/// a shared mutex so the frame renderer and input loop share ownership.
pub enum Modal {
    Model(Arc<Mutex<selectors::ModelSelector>>),
    Llama(Arc<Mutex<llama::LlamaSelector>>),
    LlamaLoadPlan {
        selector: Arc<Mutex<llama::LlamaLoadPlanSelector>>,
        client: crate::core::llama::LlamaClient,
        target: String,
        loaded: Vec<String>,
    },
    LlamaUnloadConfirm {
        selector: Arc<Mutex<llama::LlamaUnloadConfirmSelector>>,
        client: crate::core::llama::LlamaClient,
        target: String,
    },
    HuggingFace(Arc<Mutex<llama::HuggingFaceSelector>>),
    HuggingFaceDownload(Arc<Mutex<llama::HuggingFaceDownloadSelector>>),
    ScopedModels(Arc<Mutex<selectors::ScopedModelsSelector>>),
    Thinking(Arc<Mutex<selectors::ThinkingSelector>>),
    Theme(Arc<Mutex<selectors::ListSelector>>),
    /// User-message selector for `/fork`.
    Fork(Arc<Mutex<selectors::ListSelector>>),
    Settings(Arc<Mutex<SettingsPanel>>),
    /// Session picker: selector + the metadata list it was built from.
    Resume(
        Arc<Mutex<session_meta::SessionPickerState>>,
        Vec<session_meta::SessionMetaForPicker>,
    ),
    /// Project-scoped trust decision selector.
    Trust(Arc<Mutex<selectors::TrustSelector>>),
    /// Confirmation shown when `--session` resolves to another project.
    CrossProjectSession(Arc<Mutex<session_meta::CrossProjectSessionPrompt>>),
    /// Parent-linked entries from the active session.
    Tree(Arc<Mutex<tree_selector::TreeSelector>>),
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
    build_autocomplete_provider_with_skills(cwd, &[], true)
}

/// Create an autocomplete provider from the active session's loaded skills.
/// Completion must use this registry rather than scanning the filesystem so
/// it has the same resource boundary as prompt construction.
pub fn build_autocomplete_provider_with_skills(
    cwd: String,
    skills: &[crate::core::skills::Skill],
    enable_skill_commands: bool,
) -> CombinedAutocompleteProvider {
    let mut commands: Vec<SlashCommand> = slash::BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|c| {
            SlashCommand::new(
                c.name,
                Some(c.description.to_string()),
                c.argument_hint.map(|s| s.to_string()),
            )
        })
        .collect();
    if enable_skill_commands {
        commands.extend(skills.iter().map(|skill| {
            SlashCommand::new(
                format!("skill:{}", skill.name),
                Some(skill_autocomplete_description(skill)),
                None,
            )
        }));
    }
    let fd_path = std::env::var("PI_FD_PATH").ok().or_else(|| {
        std::process::Command::new("which")
            .arg("fd")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    });
    CombinedAutocompleteProvider::new(commands, cwd, fd_path)
}

fn skill_autocomplete_description(skill: &crate::core::skills::Skill) -> String {
    let scope = match skill.source_info.scope.as_str() {
        "user" => "u",
        "project" => "p",
        "temporary" => "t",
        _ => return skill.description.clone(),
    };
    format!("[{scope}] {}", skill.description)
}

/// Create the editor with a keybinding-aware surface.
pub fn create_editor(cwd: String) -> Editor {
    create_editor_with_skills(cwd, &[], true)
}

/// Create the editor with autocomplete backed by the active session skills.
pub fn create_editor_with_skills(
    cwd: String,
    skills: &[crate::core::skills::Skill],
    enable_skill_commands: bool,
) -> Editor {
    let theme = EditorTheme {
        // `InteractiveMode.init()` calls Pi's `updateEditorBorderColor()`
        // before the first settled frame. The default model thinking level is
        // medium, so constructing the editor with that border avoids one
        // visible frame of the generic muted border and matches the oracle's
        // first render.
        border_color: t::thinking_border(crate::core::model_resolver::DEFAULT_THINKING_LEVEL),
    };
    let options = EditorOptions {
        // Pi's editorPaddingX default is zero; the interactive owner applies
        // the persisted setting after construction.
        padding_x: 0,
        autocomplete_max_visible: 6,
    };
    let mut editor = Editor::new(24, theme, options);
    let provider = build_autocomplete_provider_with_skills(cwd, skills, enable_skill_commands);
    editor.set_autocomplete_provider(Box::new(provider));
    editor
}

/// Expand an explicit `/skill:name [args]` invocation using a loaded skill.
/// Unknown skills and unreadable files remain ordinary prompt text, matching
/// upstream's pass-through behavior without fabricating expanded content.
pub fn expand_skill_command(text: &str, skills: &[crate::core::skills::Skill]) -> String {
    if !text.starts_with("/skill:") {
        return text.to_string();
    }
    let (skill_name, args) = match text.find(char::is_whitespace) {
        Some(index) => (&text[7..index], text[index..].trim()),
        None => (&text[7..], ""),
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == skill_name) else {
        return text.to_string();
    };
    let Ok(content) = std::fs::read_to_string(&skill.file_path) else {
        return text.to_string();
    };
    let content = crate::core::settings::strip_bom(&content);
    let body = pi_agent::harness::frontmatter::parse_frontmatter(content)
        .map(|(_, body)| body)
        .unwrap_or_else(|| content.to_string())
        .trim()
        .to_string();
    let skill_block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.file_path, skill.base_dir, body
    );
    if args.is_empty() {
        skill_block
    } else {
        format!("{skill_block}\n\n{args}")
    }
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
    easter_egg_components: &[SharedComponent],
    pending: &str,
) -> Arc<Mutex<Scene>> {
    let pending_loader = Arc::new(Mutex::new(pi_tui::components::Loader::new(pending)));
    build_scene_with_loader(
        transcript,
        editor,
        footer_component,
        modal_component,
        easter_egg_components,
        &pending_loader,
        pending,
    )
}

/// Render the full scene while retaining one loader instance across frames.
///
/// Rebuilding a loader from inside the render loop resets its frame index on
/// every redraw, making the spinner appear frozen. The interactive owner
/// supplies a lifecycle-scoped loader here so its 80 ms animation can advance
/// independently of transcript/editor scene reconstruction.
pub fn build_scene_with_loader(
    transcript: &Arc<Mutex<Markdown>>,
    editor: &Arc<Mutex<Editor>>,
    footer_component: &Arc<Mutex<Text>>,
    modal_component: Option<SharedComponent>,
    easter_egg_components: &[SharedComponent],
    pending_loader: &Arc<Mutex<pi_tui::components::Loader>>,
    pending: &str,
) -> Arc<Mutex<Scene>> {
    let transcript_scroll_view = new_transcript_scroll_view(transcript);
    build_scene_with_loader_and_scroll_view(
        &transcript_scroll_view,
        editor,
        footer_component,
        modal_component,
        easter_egg_components,
        pending_loader,
        pending,
    )
}

/// Create the primary transcript viewport used by the interactive scene.
///
/// The official Pi interactive mode constructs this viewport once with
/// `follow: "end"`, `primary: true`, and chained overscroll, then keeps it
/// mounted while the transcript and dock contents change. Returning the
/// shared component lets the interactive owner keep the same scroll offset,
/// follow-tail state, and layout metadata across scene rebuilds.
pub fn new_transcript_scroll_view(transcript: &Arc<Mutex<Markdown>>) -> SharedComponent {
    Arc::new(Mutex::new(ScrollView::with_options(
        transcript.clone(),
        true,
        pi_tui::ScrollOverscroll::Chain,
    )))
}

/// Create the interactive document container and its retained primary
/// viewport. Hidden components/extension output are mounted in the document
/// container so they scroll with the transcript instead of stealing height
/// from the fixed composer dock.
pub fn new_interactive_document_scroll_view(
    transcript: &Arc<Mutex<Markdown>>,
) -> (Arc<Mutex<ScrollView>>, Arc<Mutex<Container>>) {
    let document = Arc::new(Mutex::new(Container::new()));
    document
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .add_child(transcript.clone() as SharedComponent);
    let scroll_view = Arc::new(Mutex::new(ScrollView::with_options(
        document.clone() as SharedComponent,
        true,
        pi_tui::ScrollOverscroll::Chain,
    )));
    (scroll_view, document)
}

/// A caller-owned transcript viewport in either its concrete or erased form.
///
/// The concrete implementation is useful to owners that need direct access
/// to scroll controls; the erased implementation is convenient when the
/// owner already stores all scene children as [`SharedComponent`] values.
pub trait TranscriptScrollViewHandle {
    /// Clone the same underlying component handle without constructing a new
    /// viewport or losing its retained state.
    fn clone_shared_component(&self) -> SharedComponent;
}

impl TranscriptScrollViewHandle for SharedComponent {
    fn clone_shared_component(&self) -> SharedComponent {
        self.clone()
    }
}

impl TranscriptScrollViewHandle for Arc<Mutex<ScrollView>> {
    fn clone_shared_component(&self) -> SharedComponent {
        self.clone()
    }
}

/// Render the full scene around a caller-owned transcript scroll view.
///
/// `transcript_scroll_view` must be created once for the interactive
/// lifecycle and supplied again for each frame. It is inserted directly as
/// the scene's growable first child; this function never wraps or replaces
/// it. The existing [`build_scene`] and [`build_scene_with_loader`] helpers
/// remain source-compatible and create their own view for legacy callers.
/// Both a [`SharedComponent`] and an `Arc<Mutex<ScrollView>>` are accepted.
pub fn build_scene_with_loader_and_scroll_view<S: TranscriptScrollViewHandle>(
    transcript_scroll_view: &S,
    editor: &Arc<Mutex<Editor>>,
    footer_component: &Arc<Mutex<Text>>,
    modal_component: Option<SharedComponent>,
    easter_egg_components: &[SharedComponent],
    pending_loader: &Arc<Mutex<pi_tui::components::Loader>>,
    pending: &str,
) -> Arc<Mutex<Scene>> {
    build_interactive_scene_with_loader_and_scroll_view(
        transcript_scroll_view,
        editor,
        footer_component,
        None,
        modal_component,
        easter_egg_components,
        pending_loader,
        pending,
    )
}

/// Build the interactive root with the same two-level topology as Pi:
/// a growable transcript viewport followed by a shrinkable dock containing
/// pending/status widgets, the editor, and the footer. The status component
/// is optional for source compatibility with callers that only need the
/// lower-level scene helper.
#[allow(clippy::too_many_arguments)]
pub fn build_interactive_scene_with_loader_and_scroll_view<S: TranscriptScrollViewHandle>(
    transcript_scroll_view: &S,
    editor: &Arc<Mutex<Editor>>,
    footer_component: &Arc<Mutex<Text>>,
    status_component: Option<&Arc<Mutex<Text>>>,
    modal_component: Option<SharedComponent>,
    easter_egg_components: &[SharedComponent],
    pending_loader: &Arc<Mutex<pi_tui::components::Loader>>,
    pending: &str,
) -> Arc<Mutex<Scene>> {
    let transcript = transcript_scroll_view.clone_shared_component();
    let mut dock_entries: Vec<StackLayoutEntry> = Vec::new();
    // Scene truncation keeps the top of an overfull scene. Render the newest
    // hidden component first so repeated/triggered announcements remain
    // visible in a narrow terminal, matching the chat container's bottom
    // anchored viewport.
    if !pending.is_empty() {
        let mut loader = pending_loader
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if loader.message() != pending {
            loader.set_message(pending);
        }
        if !loader.is_running() {
            loader.start();
        }
        drop(loader);
        dock_entries.push(
            StackLayoutEntry::new(pending_loader.clone() as SharedComponent)
                .with_shrink(1)
                .with_min_size(0),
        );
    } else {
        pending_loader
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stop();
    }
    if let Some(status) = status_component {
        dock_entries.push(
            StackLayoutEntry::new(status.clone() as SharedComponent)
                .with_shrink(1)
                .with_min_size(0),
        );
    }
    // Pi always keeps one leading spacer in the widget-above container, even
    // when no extension widget is registered.  Easter-egg widgets share that
    // container, so preserve the blank row before them and before the editor.
    dock_entries.push(
        StackLayoutEntry::new(Arc::new(Mutex::new(Spacer::new(1))) as SharedComponent)
            .with_shrink(1)
            .with_min_size(0),
    );
    for component in easter_egg_components.iter().rev().cloned() {
        dock_entries.push(
            StackLayoutEntry::new(component)
                .with_shrink(1)
                .with_min_size(0),
        );
    }
    let editor_or_modal = modal_component.unwrap_or_else(|| editor.clone() as SharedComponent);
    // Pi swaps the editor container's child for a dialog/selector while a
    // modal is active. Keeping that component in the same dock slot avoids
    // rendering a hidden editor beneath the selector and preserves the
    // editor's horizontal-only border surface.
    dock_entries.push(
        StackLayoutEntry::new(editor_or_modal)
            .with_shrink(1)
            .with_min_size(3),
    );
    dock_entries.push(
        StackLayoutEntry::new(footer_component.clone() as SharedComponent)
            .with_shrink(1)
            .with_min_size(1),
    );

    let dock: SharedComponent = Arc::new(Mutex::new(VStack::with_layout_entries(dock_entries)));
    let children = vec![transcript.clone(), dock.clone()];
    let root_entries = vec![
        StackLayoutEntry::new(transcript)
            .with_basis(LayoutBasis::Cells(0))
            .with_grow(1)
            .with_shrink(1)
            .with_min_size(1),
        StackLayoutEntry::new(dock)
            .with_basis(LayoutBasis::Auto)
            .with_grow(0)
            .with_shrink(1)
            .with_min_size(1),
    ];
    Arc::new(Mutex::new(Scene::with_layout_entries(
        children,
        root_entries,
    )))
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
                return SubmitAction::Command(
                    command,
                    (!argument.is_empty()).then(|| argument.to_string()),
                );
            }
            // Upstream keeps these Easter eggs out of the public command
            // registry, but exact no-argument invocations still execute.
            if argument.is_empty() {
                if let Some(command) = slash::find_hidden_command(name) {
                    return SubmitAction::Command(command, None);
                }
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
pub fn compose_transcript(
    messages: &[AgentMessage],
    hide_thinking: bool,
    stream_text: &str,
) -> String {
    compose_transcript_with_cache_notices(messages, hide_thinking, stream_text, &[])
}

/// Compose the current transcript while re-injecting derived cache notices.
/// Notices are deliberately not persisted as messages; they are recomputed
/// from session usage whenever the setting is enabled.
pub fn compose_transcript_with_cache_notices(
    messages: &[AgentMessage],
    hide_thinking: bool,
    stream_text: &str,
    cache_notices: &[(u64, String)],
) -> String {
    compose_transcript_with_cache_notices_and_options(
        messages,
        messages::TranscriptRenderOptions {
            hide_thinking,
            ..Default::default()
        },
        stream_text,
        cache_notices,
    )
}

/// Compose a transcript with explicit live rendering controls.
pub fn compose_transcript_with_cache_notices_and_options(
    messages: &[AgentMessage],
    options: messages::TranscriptRenderOptions,
    stream_text: &str,
    cache_notices: &[(u64, String)],
) -> String {
    let mut text =
        messages::build_transcript_with_cache_notices_and_options(messages, options, cache_notices);
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
    let (provider, id) = parse_model_selection(value)?;
    settings.set_default_model_and_provider(provider.clone(), id.clone());
    Some((provider, id))
}

/// Parse a canonical provider/model reference without changing persisted
/// settings. Ordinary `/model` selection and Ctrl+P cycling are session-only;
/// only the selector's explicit Ctrl+S action should call
/// [`apply_model_selection`].
pub fn parse_model_selection(value: &str) -> Option<(String, String)> {
    let (provider, id) = value.trim().split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some((provider.to_string(), id.to_string()))
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
    use std::sync::{Arc, Mutex};

    use pi_tui::components::{Editor, EditorOptions, EditorTheme, Markdown, ScrollView, Text};
    use pi_tui::tui::SharedComponent;

    use crate::interactive::footer::format_cwd_for_footer;
    use crate::interactive::messages::{build_transcript, format_tokens};
    use crate::interactive::slash::{find_command, parse_invocation, BUILTIN_SLASH_COMMANDS};
    use crate::interactive::{
        build_interactive_scene_with_loader_and_scroll_view,
        build_scene_with_loader_and_scroll_view, create_editor, new_transcript_scroll_view,
        parse_submit, SubmitAction,
    };

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
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
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn parse_submit_executes_hidden_commands_without_publishing_them() {
        for name in ["debug", "arminsayshi", "dementedelves"] {
            match parse_submit(&format!("/{name}")) {
                SubmitAction::Command(command, argument) => {
                    assert_eq!(command.name, name);
                    assert!(argument.is_none());
                }
                SubmitAction::Prompt(prompt) => panic!("hidden command became prompt: {prompt}"),
            }
            assert!(find_command(name).is_none());
        }
        assert!(matches!(
            parse_submit("/debug extra"),
            SubmitAction::Prompt(_)
        ));
    }

    #[test]
    fn slash_registry_covers_builtins() {
        for name in [
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
        ] {
            assert!(find_command(name).is_some(), "missing {name}");
        }
        let names: Vec<&str> = BUILTIN_SLASH_COMMANDS.iter().map(|c| c.name).collect();
        assert!(names.contains(&"hotkeys"));
        assert_eq!(names.len(), 23);
        for removed in ["help", "theme", "clear", "llama"] {
            assert!(
                !names.contains(&removed),
                "unexpected public builtin {removed}"
            );
            assert!(matches!(
                parse_submit(&format!("/{removed}")),
                SubmitAction::Prompt(_)
            ));
        }
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
        assert_eq!(
            format_cwd_for_footer("/home/user/projects", Some("/home/user")),
            "~/projects"
        );
        assert_eq!(
            format_cwd_for_footer("/opt/elsewhere", Some("/home/user")),
            "/opt/elsewhere"
        );
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
    fn editor_starts_with_the_default_thinking_border() {
        crate::interactive::tui_theme::load_theme(crate::theme::DEFAULT_THEME);
        let editor = create_editor(".".to_string());
        assert_eq!(
            (editor.border_color)("─"),
            crate::interactive::tui_theme::thinking_border("medium")("─")
        );
    }

    #[test]
    fn transcript_is_empty_without_messages() {
        let text = build_transcript(&[], false);
        assert!(text.is_empty());
    }

    #[test]
    fn scene_builder_reuses_the_caller_owned_transcript_scroll_view() {
        let transcript = Arc::new(Mutex::new(Markdown::new(
            "",
            1,
            0,
            crate::interactive::tui_theme::markdown_theme(),
            None,
            None,
        )));
        let transcript_scroll_view = new_transcript_scroll_view(&transcript);
        let editor = Arc::new(Mutex::new(Editor::new(
            24,
            EditorTheme {
                border_color: Arc::new(|line| line.to_string()),
            },
            EditorOptions {
                padding_x: 1,
                autocomplete_max_visible: 6,
            },
        )));
        let footer = Arc::new(Mutex::new(Text::new("", 0, 0, None)));
        let loader = Arc::new(Mutex::new(pi_tui::components::Loader::new("")));

        let first_scene = build_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer,
            None,
            &[],
            &loader,
            "",
        );
        let first_child = first_scene
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .children[0]
            .clone();
        assert!(Arc::ptr_eq(&first_child, &transcript_scroll_view));

        let second_scene = build_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer,
            None,
            &[],
            &loader,
            "",
        );
        let second_child = second_scene
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .children[0]
            .clone();
        assert!(Arc::ptr_eq(&second_child, &transcript_scroll_view));
        assert!(Arc::ptr_eq(&first_child, &second_child));

        let typed_scroll_view = Arc::new(Mutex::new(ScrollView::with_options(
            transcript,
            true,
            pi_tui::ScrollOverscroll::Chain,
        )));
        let typed_shared: SharedComponent = typed_scroll_view.clone();
        let typed_scene = build_scene_with_loader_and_scroll_view(
            &typed_scroll_view,
            &editor,
            &footer,
            None,
            &[],
            &loader,
            "",
        );
        let typed_child = typed_scene
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .children[0]
            .clone();
        assert!(Arc::ptr_eq(&typed_child, &typed_shared));
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn interactive_document_keeps_announcement_in_scroll_view_and_dock_fixed() {
        let transcript = Arc::new(Mutex::new(Markdown::new(
            "prompt\nresponse",
            1,
            0,
            crate::interactive::tui_theme::markdown_theme(),
            None,
            None,
        )));
        let (transcript_scroll_view, document) =
            crate::interactive::new_interactive_document_scroll_view(&transcript);
        document
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .add_child(crate::interactive::easter_eggs::armin_component());
        document
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .add_child(crate::interactive::easter_eggs::armin_component());
        let editor = Arc::new(Mutex::new(Editor::new(
            24,
            EditorTheme {
                border_color: Arc::new(|line| line.to_string()),
            },
            EditorOptions::default(),
        )));
        let footer = Arc::new(Mutex::new(Text::new("footer", 0, 0, None)));
        let status = Arc::new(Mutex::new(Text::new("status", 1, 0, None)));
        let loader = Arc::new(Mutex::new(pi_tui::components::Loader::new("")));

        let scene = build_interactive_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer,
            Some(&status),
            None,
            &[],
            &loader,
            "",
        );
        let _ = pi_tui::render_layout_frame(scene as SharedComponent, 110, 34);
        let scene = build_interactive_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer,
            Some(&status),
            None,
            &[],
            &loader,
            "",
        );
        let _ = pi_tui::render_layout_frame(scene as SharedComponent, 38, 12);
        let scene = build_interactive_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer,
            Some(&status),
            None,
            &[],
            &loader,
            "",
        );
        let _ = pi_tui::render_layout_frame(scene as SharedComponent, 110, 34);

        document
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .add_child(crate::interactive::easter_eggs::earendil_component());
        let scene = build_interactive_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer,
            Some(&status),
            None,
            &[],
            &loader,
            "",
        );
        let frame = pi_tui::render_layout_frame(scene as SharedComponent, 110, 34);
        assert_eq!(frame.root.children.len(), 2);
        assert!(frame
            .lines
            .iter()
            .any(|line| line.contains("pi has joined Earendil")));
        assert!(frame.root.children[1].rect.height >= 3);
        assert_eq!(
            frame
                .primary_scroll_view
                .as_ref()
                .expect("primary transcript viewport")
                .viewport_height(),
            frame.root.children[0].rect.height
        );
    }
}
