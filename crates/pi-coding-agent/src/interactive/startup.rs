//! Startup-only TUI surfaces — ports of `cli/startup-ui.ts` and
//! `first-time-setup.ts`.
//!
//! These components intentionally own only the short startup interaction.
//! The regular interactive mode keeps its existing terminal/event lifecycle;
//! this module hands it a clean terminal after setup has either been saved or
//! cancelled.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use pi_tui::components::select_list::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::components::{Input, Spacer, Text};
use pi_tui::controller::TuiStopOptions;
use pi_tui::keys::TuiKey;
use pi_tui::terminal::{TerminalBackend, TerminalEvent};
use pi_tui::tui::{Component, SharedComponent};
use pi_tui::TuiMainScreen;

use crate::args::Args;
use crate::core::extensions::{Extension, ExtensionLoadError, ResourceDiscovery};
use crate::core::prompt_templates::PromptTemplate;
use crate::core::settings::SettingsManager;
use crate::interactive::tui_theme;

const SETUP_LOGO_LINES: &str = "██████\n██  ██\n████  ██\n██    ██";

const THEME_OPTIONS: [(&str, &str); 2] = [("dark", "Dark"), ("light", "Light")];
const ANALYTICS_OPTIONS: [(bool, &str); 2] =
    [(true, "Share anonymous usage data"), (false, "Don't share")];

/// The startup header and loaded-resource list shown above the transcript.
///
/// Pi keeps the header and resource list as separate expandable components.
/// Rust's interactive document container is intentionally smaller, so this
/// component preserves their ordering and independent padding while sharing
/// one expansion state. The state is changed by the interactive owner when
/// `Ctrl+O` is pressed; handling the key here as well keeps the component
/// useful to embedders and deterministic component tests.
pub struct InteractiveStartupPresentation {
    header_collapsed: String,
    header_expanded: String,
    resources_collapsed: String,
    resources_expanded: String,
    context_leading_spacer: bool,
    expanded: bool,
}

impl InteractiveStartupPresentation {
    /// Build the startup presentation from the same resources used to build
    /// the interactive system prompt. This keeps the visible resource list
    /// honest: a resource is listed only after the Rust loader can discover
    /// it, and disabled resource classes stay hidden just like Pi's UI.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: &str,
        cwd: &str,
        agent_dir: &str,
        args: &Args,
        settings: &SettingsManager,
        resources: &ResourceDiscovery,
        extensions: &[Extension],
        extension_errors: &[ExtensionLoadError],
        prompt_templates: &[PromptTemplate],
        expanded: bool,
    ) -> Self {
        let mut presentation = Self {
            header_collapsed: String::new(),
            header_expanded: String::new(),
            resources_collapsed: String::new(),
            resources_expanded: String::new(),
            context_leading_spacer: false,
            expanded,
        };
        presentation.refresh(
            version,
            cwd,
            agent_dir,
            args,
            settings,
            resources,
            extensions,
            extension_errors,
            prompt_templates,
        );
        presentation
    }

    /// Refresh loaded-resource text after `/reload` or a session switch.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh(
        &mut self,
        version: &str,
        cwd: &str,
        agent_dir: &str,
        args: &Args,
        settings: &SettingsManager,
        resources: &ResourceDiscovery,
        extensions: &[Extension],
        extension_errors: &[ExtensionLoadError],
        prompt_templates: &[PromptTemplate],
    ) {
        let (header_collapsed, header_expanded) = startup_header(version);
        self.header_collapsed = header_collapsed;
        self.header_expanded = header_expanded;
        let summary = StartupResourceSummary::load(
            cwd,
            agent_dir,
            args,
            settings,
            resources,
            extensions,
            extension_errors,
            prompt_templates,
        );
        self.context_leading_spacer = !summary.context.is_empty();
        self.resources_collapsed = summary.render(false);
        self.resources_expanded = summary.render(true);
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
    }

    /// Toggle the shared startup/tool expansion state and return the new
    /// state, matching the upstream `setToolsExpanded` transition.
    pub fn toggle(&mut self) -> bool {
        self.expanded = !self.expanded;
        self.expanded
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    fn active_header(&self) -> &str {
        if self.expanded {
            &self.header_expanded
        } else {
            &self.header_collapsed
        }
    }

    fn active_resources(&self) -> &str {
        if self.expanded {
            &self.resources_expanded
        } else {
            &self.resources_collapsed
        }
    }
}

impl Component for InteractiveStartupPresentation {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        lines.extend(Text::new(self.active_header(), 1, 0, None).render(width));
        lines.push(String::new());
        let resources = self.active_resources();
        if !resources.is_empty() {
            // The upstream document mounts the context list in its own
            // container with a leading spacer. The header's trailing spacer
            // therefore leaves two blank rows before the first resource
            // section; retain that geometry in the merged Rust component.
            if self.context_leading_spacer {
                lines.push(String::new());
            }
            lines.extend(Text::new(resources, 0, 0, None).render(width));
            // The upstream loaded-resources container also owns a trailing
            // spacer before the transcript/composer boundary.
            lines.push(String::new());
        }
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        if pi_tui::match_key(key, "ctrl+o") {
            self.toggle();
        }
    }
}

fn startup_header(version: &str) -> (String, String) {
    let logo = format!(
        "{}{}",
        tui_theme::bold(tui_theme::fg("accent", crate::config::APP_NAME)),
        tui_theme::fg("dim", format!(" v{version}")),
    );
    let compact = [
        startup_hint("escape", "interrupt"),
        startup_hint("ctrl+c/ctrl+d", "clear/exit"),
        startup_hint("/", "commands"),
        startup_hint("!", "bash"),
        startup_hint("ctrl+o", "more"),
    ]
    .join(&tui_theme::fg("muted", " · "));
    let compact_onboarding = tui_theme::fg(
        "dim",
        "Press ctrl+o to show full startup help and loaded resources.",
    );
    let onboarding = tui_theme::fg(
        "dim",
        "Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.",
    );
    let collapsed = format!("{logo}\n{compact}\n{compact_onboarding}\n\n{onboarding}");

    let expanded_instructions = [
        startup_hint("escape", "to interrupt"),
        startup_hint("ctrl+c", "to clear"),
        startup_hint("ctrl+c twice", "to exit"),
        startup_hint("ctrl+d", "to exit (empty)"),
        startup_hint("ctrl+z", "to suspend"),
        startup_hint("ctrl+k", "to delete to end"),
        startup_hint("shift+tab", "to cycle thinking level"),
        startup_hint("ctrl+p/shift+ctrl+p", "to cycle models"),
        startup_hint("ctrl+l", "to select model"),
        startup_hint("ctrl+o", "to expand tools"),
        startup_hint("ctrl+t", "to expand thinking"),
        startup_hint("ctrl+g", "for external editor"),
        startup_hint("/", "for commands"),
        startup_hint("!", "to run bash"),
        startup_hint("!!", "to run bash (no context)"),
        startup_hint("alt+enter", "to queue follow-up"),
        startup_hint("alt+up", "to edit all queued messages"),
        startup_hint("ctrl+v", "to paste image (with text fallback)"),
        startup_hint("drop files", "to attach"),
    ]
    .join("\n");
    let expanded = format!("{logo}\n{expanded_instructions}\n\n{onboarding}");
    (collapsed, expanded)
}

fn startup_hint(key: &str, description: &str) -> String {
    format!(
        "{}{}",
        tui_theme::fg("dim", key),
        tui_theme::fg("muted", format!(" {description}")),
    )
}

#[derive(Debug, Default)]
struct StartupResourceSummary {
    cwd: String,
    context: Vec<String>,
    skills: Vec<(String, String)>,
    prompts: Vec<(String, String)>,
    extensions: Vec<String>,
    themes: Vec<String>,
    issues: Vec<String>,
}

impl StartupResourceSummary {
    #[allow(clippy::too_many_arguments)]
    fn load(
        cwd: &str,
        agent_dir: &str,
        args: &Args,
        settings: &SettingsManager,
        resources: &ResourceDiscovery,
        extensions: &[Extension],
        extension_errors: &[ExtensionLoadError],
        prompt_templates: &[PromptTemplate],
    ) -> Self {
        let context = if args.no_context_files {
            Vec::new()
        } else {
            crate::core::context_files::load_project_context_files(cwd, agent_dir)
                .into_iter()
                .map(|file| file.path)
                .collect()
        };

        let mut summary = Self {
            cwd: cwd.to_string(),
            context,
            ..Self::default()
        };

        if !args.no_skills {
            let mut skill_paths = settings.get_skill_paths();
            skill_paths.extend(args.skills.iter().cloned());
            skill_paths.extend(resources.resolved_skill_paths(cwd));
            let (skills, diagnostics) =
                crate::core::skills::load_skills(crate::core::skills::LoadSkillsOptions {
                    cwd: cwd.to_string(),
                    agent_dir: agent_dir.to_string(),
                    skill_paths,
                });
            summary.skills = skills
                .into_iter()
                .map(|skill| (skill.name, skill.file_path))
                .collect();
            summary
                .issues
                .extend(diagnostics.into_iter().map(|diagnostic| {
                    format!(
                        "skill: {}{}",
                        diagnostic.message,
                        diagnostic
                            .path
                            .map(|path| format!(" ({path})"))
                            .unwrap_or_default()
                    )
                }));
        }

        if !args.no_prompt_templates {
            summary.prompts = prompt_templates
                .iter()
                .map(|template| (format!("/{}", template.name), template.file_path.clone()))
                .collect();
        }

        summary.extensions = extensions
            .iter()
            .filter(|extension| !extension.hidden)
            .map(|extension| {
                if extension.resolved_path.is_empty() {
                    extension.path.clone()
                } else {
                    extension.resolved_path.clone()
                }
            })
            .collect();

        if !args.no_themes {
            let mut theme_paths = settings.get_theme_paths();
            theme_paths.extend(args.themes.iter().cloned());
            theme_paths.extend(resources.resolved_theme_paths(cwd));
            summary.themes = theme_paths;
        }

        summary.issues.extend(
            extension_errors
                .iter()
                .map(|error| format!("extension: {} ({})", error.error, error.path)),
        );
        summary
    }

    fn render(&self, expanded: bool) -> String {
        let mut sections = Vec::new();
        if !self.context.is_empty() {
            let compact = self
                .context
                .iter()
                .map(|path| display_path(path, &self.cwd))
                .collect::<Vec<_>>();
            sections.push(render_resource_section(
                "Context",
                compact,
                self.context
                    .iter()
                    .map(|path| display_path(path, ""))
                    .collect(),
                expanded,
            ));
        }
        if !self.skills.is_empty() {
            let mut compact = self
                .skills
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            compact.sort();
            let mut full = self
                .skills
                .iter()
                .map(|(name, path)| format!("{name} {}", display_path(path, "")))
                .collect::<Vec<_>>();
            full.sort();
            sections.push(render_resource_section("Skills", compact, full, expanded));
        }
        if !self.prompts.is_empty() {
            let mut compact = self
                .prompts
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            compact.sort();
            let mut full = self
                .prompts
                .iter()
                .map(|(name, path)| format!("{name} {}", display_path(path, "")))
                .collect::<Vec<_>>();
            full.sort();
            sections.push(render_resource_section("Prompts", compact, full, expanded));
        }
        if !self.extensions.is_empty() {
            let mut compact = self
                .extensions
                .iter()
                .map(|path| compact_path(path))
                .collect::<Vec<_>>();
            compact.sort();
            let mut full = self
                .extensions
                .iter()
                .map(|path| display_path(path, ""))
                .collect::<Vec<_>>();
            full.sort();
            sections.push(render_resource_section(
                "Extensions",
                compact,
                full,
                expanded,
            ));
        }
        if !self.themes.is_empty() {
            let mut compact = self
                .themes
                .iter()
                .map(|path| compact_path(path))
                .collect::<Vec<_>>();
            compact.sort();
            let mut full = self
                .themes
                .iter()
                .map(|path| display_path(path, ""))
                .collect::<Vec<_>>();
            full.sort();
            sections.push(render_resource_section("Themes", compact, full, expanded));
        }
        if !self.issues.is_empty() {
            let mut issues = self.issues.clone();
            issues.sort();
            sections.push(render_resource_section(
                "Startup issues",
                issues.clone(),
                issues,
                true,
            ));
        }
        sections.join("\n\n")
    }
}

fn render_resource_section(
    name: &str,
    compact: Vec<String>,
    full: Vec<String>,
    expanded: bool,
) -> String {
    let values = if expanded { full } else { compact };
    let values = values
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| tui_theme::fg("dim", format!("  {value}")))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return String::new();
    }
    format!(
        "{}\n{}",
        tui_theme::fg("mdHeading", format!("[{name}]")),
        values.join("\n")
    )
}

fn compact_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string())
}

fn display_path(path: &str, cwd: &str) -> String {
    let normalized = path.replace('\\', "/");
    let cwd = cwd.replace('\\', "/");
    if !cwd.is_empty() {
        let prefix = format!("{cwd}/");
        if let Some(relative) = normalized.strip_prefix(&prefix) {
            return relative.to_string();
        }
        if normalized == cwd {
            return ".".to_string();
        }
    }
    let Some(home) = crate::config::home_dir() else {
        return normalized;
    };
    let home = home.to_string_lossy().replace('\\', "/");
    if normalized == home {
        "~".to_string()
    } else if let Some(relative) = normalized.strip_prefix(&format!("{home}/")) {
        format!("~/{relative}")
    } else {
        normalized
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StartupEvent {
    Confirm,
    Cancel,
    Input(String),
}

/// A value/label pair accepted by [`show_startup_selector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupOption<T> {
    pub label: String,
    pub value: T,
}

impl<T> StartupOption<T> {
    pub fn new(label: impl Into<String>, value: T) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstTimeSetupResult {
    pub theme: String,
    pub share_analytics: bool,
}

fn select_theme() -> pi_tui::components::select_list::SelectListTheme {
    pi_tui::components::select_list::SelectListTheme {
        selected_prefix: Box::new(|s| tui_theme::fg("accent", s)),
        selected_text: Box::new(|s| tui_theme::bg("selectedBg", tui_theme::fg("selectedText", s))),
        description: Box::new(|s| tui_theme::fg("muted", s)),
        scroll_info: Box::new(|s| tui_theme::fg("muted", s)),
        no_match: Box::new(|s| tui_theme::fg("warning", s)),
    }
}

fn text_lines(text: impl Into<String>, width: usize) -> Vec<String> {
    Text::new(text, 1, 0, None).render(width)
}

fn border(width: usize, top: bool) -> String {
    match width {
        0 => String::new(),
        1 => (if top { '╭' } else { '╰' }).to_string(),
        width => {
            if top {
                format!("╭{}╮", "─".repeat(width - 2))
            } else {
                format!("╰{}╯", "─".repeat(width - 2))
            }
        }
    }
}

fn key_hint(confirm: &str) -> String {
    format!("↑↓ navigate  Enter {confirm}  Esc cancel")
}

/// Selector used by the startup helpers. It is a normal pi-tui component,
/// rather than a line-oriented prompt, so selection and cancellation travel
/// through the same controller dispatch path as the rest of the TUI.
pub struct StartupSelectorComponent {
    title: String,
    list: SelectList,
}

impl StartupSelectorComponent {
    fn new(title: impl Into<String>, list: SelectList) -> Self {
        Self {
            title: title.into(),
            list,
        }
    }

    fn selected_index(&self) -> usize {
        self.list.selected_index()
    }
}

impl Component for StartupSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![border(width, true)];
        lines.extend(text_lines(
            tui_theme::bold(tui_theme::fg("accent", &self.title)),
            width,
        ));
        lines.push(Spacer::new(1).render(width)[0].clone());
        lines.extend(self.list.render(width.saturating_sub(2)));
        lines.extend(text_lines(
            tui_theme::fg("muted", key_hint("confirm")),
            width,
        ));
        lines.push(border(width, false));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        self.list.handle_input(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.list.set_focused(focused);
    }
}

/// Two-step first-run setup component. The selected theme is previewed as
/// soon as the highlighted row changes; analytics is only persisted after the
/// second confirmation reaches the outer startup transaction.
pub struct FirstTimeSetupComponent {
    step: SetupStep,
    detected_theme: String,
    theme_index: usize,
    analytics_index: usize,
    list: SelectList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Theme,
    Analytics,
}

impl FirstTimeSetupComponent {
    fn new(detected_theme: &str, sender: mpsc::Sender<StartupEvent>) -> Self {
        let theme_index = THEME_OPTIONS
            .iter()
            .position(|(value, _)| *value == detected_theme)
            .unwrap_or(0);
        let preview_sender = sender.clone();
        let cancel_sender = sender.clone();
        let list = SelectList::new(
            THEME_OPTIONS
                .iter()
                .map(|(value, label)| SelectItem::new(*value, *label, None))
                .collect(),
            4,
            select_theme(),
            SelectListLayoutOptions::default(),
        )
        .with_callbacks(
            move |_| {
                let _ = sender.send(StartupEvent::Confirm);
            },
            move || {
                let _ = cancel_sender.send(StartupEvent::Cancel);
            },
            move |item| {
                let _ = preview_sender.send(StartupEvent::Input(item.value.clone()));
            },
        );
        let mut component = Self {
            step: SetupStep::Theme,
            detected_theme: detected_theme.to_string(),
            theme_index,
            analytics_index: 0,
            list,
        };
        component.list.set_selected_index(theme_index);
        component
    }

    fn replace_list(
        &mut self,
        items: Vec<SelectItem>,
        sender: mpsc::Sender<StartupEvent>,
        cancel_sender: mpsc::Sender<StartupEvent>,
    ) {
        self.list = SelectList::new(items, 4, select_theme(), SelectListLayoutOptions::default())
            .with_callbacks(
                move |_| {
                    let _ = sender.send(StartupEvent::Confirm);
                },
                move || {
                    let _ = cancel_sender.send(StartupEvent::Cancel);
                },
                |_| {},
            );
    }

    fn advance_to_analytics(&mut self, sender: mpsc::Sender<StartupEvent>) {
        self.theme_index = self.list.selected_index();
        self.step = SetupStep::Analytics;
        let cancel_sender = sender.clone();
        self.replace_list(
            ANALYTICS_OPTIONS
                .iter()
                .map(|(value, label)| SelectItem::new(value.to_string(), *label, None))
                .collect(),
            sender,
            cancel_sender,
        );
        self.list.set_selected_index(self.analytics_index);
    }

    fn result(&self) -> FirstTimeSetupResult {
        FirstTimeSetupResult {
            theme: THEME_OPTIONS[self.theme_index].0.to_string(),
            share_analytics: ANALYTICS_OPTIONS[self.list.selected_index()].0,
        }
    }

    fn preview_theme(&mut self, value: &str) {
        if self.step == SetupStep::Theme {
            if let Some(index) = THEME_OPTIONS.iter().position(|(name, _)| *name == value) {
                self.theme_index = index;
                let _ = tui_theme::try_load_theme(value);
            }
        }
    }
}

impl Component for FirstTimeSetupComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![border(width, true)];
        lines.extend(text_lines(tui_theme::fg("accent", SETUP_LOGO_LINES), width));
        lines.extend(text_lines(
            tui_theme::bold(tui_theme::fg(
                "accent",
                "Welcome to pi, the minimal coding agent.",
            )),
            width,
        ));
        lines.extend(text_lines(
            tui_theme::fg(
                "text",
                match self.step {
                    SetupStep::Theme => "Pick a theme.",
                    SetupStep::Analytics => "Opt-in to anonymous usage data sharing?",
                },
            ),
            width,
        ));
        if self.step == SetupStep::Theme {
            lines.extend(text_lines(
                tui_theme::fg(
                    "muted",
                    format!("Detected system appearance: {}", self.detected_theme),
                ),
                width,
            ));
        } else {
            lines.extend(text_lines(
                tui_theme::fg(
                    "muted",
                    "Opting in stores a tracking identifier in settings.json and enables anonymous\nusage analytics. You can observe what is shared using /privacy and change\nit anytime in settings.json.",
                ),
                width,
            ));
        }
        lines.push(Spacer::new(1).render(width)[0].clone());
        lines.extend(self.list.render(width.saturating_sub(2)));
        lines.extend(text_lines(
            tui_theme::fg(
                "muted",
                key_hint(if self.step == SetupStep::Theme {
                    "continue"
                } else {
                    "finish"
                }),
            ),
            width,
        ));
        lines.push(border(width, false));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        self.list.handle_input(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.list.set_focused(focused);
    }
}

struct StartupTerminalGuard {
    terminal: Arc<Mutex<TerminalBackend>>,
}

impl Drop for StartupTerminalGuard {
    fn drop(&mut self) {
        if let Ok(mut terminal) = self.terminal.lock() {
            if terminal.is_raw() {
                let _ = terminal.leave_raw();
            }
        }
    }
}

fn configure_startup_tui(ui: &mut TuiMainScreen, settings: &SettingsManager) {
    // Startup dialogs use the same terminal policy as the main interactive
    // screen. In particular, a persisted clear-on-shrink=false setting must
    // not be replaced by a hard-coded cleanup policy while the selector is
    // open, and hardware-cursor visibility must survive the startup handoff.
    ui.set_clear_on_shrink(settings.get_clear_on_shrink());
    ui.set_show_hardware_cursor(settings.get_show_hardware_cursor());
}

fn setup_startup_tui(
    component: SharedComponent,
    settings: &SettingsManager,
) -> Result<(TuiMainScreen, StartupTerminalGuard), String> {
    let terminal = Arc::new(Mutex::new(TerminalBackend::new()));
    let mut ui = TuiMainScreen::new(terminal.clone());
    configure_startup_tui(&mut ui, settings);
    ui.add_child(component.clone());
    ui.set_focus(Some(component));
    ui.start()
        .map_err(|error| format!("start startup TUI: {error}"))?;
    Ok((ui, StartupTerminalGuard { terminal }))
}

async fn finish_startup_tui(ui: &mut TuiMainScreen) -> Result<(), String> {
    ui.clear();
    ui.render_now(true);
    // The upstream startup cleanup yields while the terminal has a chance to
    // apply the clear frame. Do not block the async runtime during that
    // settle window; startup selectors can share it with other tasks.
    tokio::time::sleep(Duration::from_millis(25)).await;
    ui.stop(TuiStopOptions::default())
        .map_err(|error| format!("restore terminal after startup TUI: {error}"))
}

fn next_startup_event(
    ui: &mut TuiMainScreen,
    receiver: &mpsc::Receiver<StartupEvent>,
) -> Result<StartupEvent, String> {
    loop {
        let event = ui
            .terminal()
            .lock()
            .map_err(|_| "startup terminal lock poisoned".to_string())?
            .next_event()
            .map_err(|error| format!("read startup terminal: {error}"))?;
        match event {
            TerminalEvent::Resize(width, height) => {
                ui.dispatch_event(TerminalEvent::Resize(width, height));
            }
            TerminalEvent::Key(raw) if !raw.is_empty() => {
                if ui
                    .terminal()
                    .lock()
                    .map_err(|_| "startup terminal lock poisoned".to_string())?
                    .consume_cell_size_response(&raw)
                {
                    continue;
                }
                ui.dispatch_raw(&raw);
                ui.render_now(false);
                if let Ok(action) = receiver.try_recv() {
                    return Ok(action);
                }
            }
            TerminalEvent::Key(raw) => {
                // On Unix, an empty key event is also used for read timeouts,
                // so only terminate when the terminal explicitly reports EOF.
                // This prevents a closed stdin from leaving startup selectors
                // spinning forever without changing normal timeout behavior.
                if raw.is_empty()
                    && ui
                        .terminal()
                        .lock()
                        .map_err(|_| "startup terminal lock poisoned".to_string())?
                        .stdin_eof()
                {
                    return Ok(StartupEvent::Cancel);
                }
            }
        }
    }
}

fn initialize_theme(settings: &SettingsManager) -> String {
    let detected = crate::theme::default_theme();
    let selected = crate::theme::resolve_theme_setting(settings.get_theme_setting(), &detected)
        .unwrap_or(detected);
    tui_theme::load_theme(&selected);
    selected
}

/// Show a one-shot startup selector and restore the terminal before returning.
pub async fn show_startup_selector<T: Clone + Send + 'static>(
    settings: &SettingsManager,
    title: impl Into<String>,
    options: Vec<StartupOption<T>>,
) -> Result<Option<T>, String> {
    if options.is_empty() {
        return Ok(None);
    }
    initialize_theme(settings);
    let (sender, receiver) = mpsc::channel();
    let settled = Arc::new(AtomicBool::new(false));
    let finish_sender = sender.clone();
    let finish_settled = settled.clone();
    let items = options
        .iter()
        .enumerate()
        .map(|(index, option)| SelectItem::new(index.to_string(), option.label.clone(), None))
        .collect();
    let list = SelectList::new(
        items,
        12,
        select_theme(),
        SelectListLayoutOptions::default(),
    )
    .with_callbacks(
        move |_| {
            let _ = finish_sender.send(StartupEvent::Confirm);
        },
        move || {
            if finish_settled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = sender.send(StartupEvent::Cancel);
            }
        },
        |_| {},
    );
    let component = Arc::new(Mutex::new(StartupSelectorComponent::new(title, list)));
    let (mut ui, _guard) = setup_startup_tui(component.clone(), settings)?;
    let result = match next_startup_event(&mut ui, &receiver)? {
        StartupEvent::Confirm => {
            if settled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                None
            } else {
                let index = component
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .selected_index();
                options.get(index).map(|option| option.value.clone())
            }
        }
        StartupEvent::Cancel | StartupEvent::Input(_) => None,
    };
    finish_startup_tui(&mut ui).await?;
    Ok(result)
}

/// Show a one-shot startup text input using pi-tui's `Input` component.
pub async fn show_startup_input(
    settings: &SettingsManager,
    title: impl Into<String>,
    placeholder: Option<&str>,
) -> Result<Option<String>, String> {
    initialize_theme(settings);
    let title = title.into();
    let (sender, receiver) = mpsc::channel();
    let settled = Arc::new(AtomicBool::new(false));
    let submit_sender = sender.clone();
    let submit_settled = settled.clone();
    let escape_sender = sender.clone();
    let escape_settled = settled.clone();
    let input = Input::new("❯ ")
        .with_submit_callback(move |value| {
            if submit_settled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = submit_sender.send(StartupEvent::Input(value.to_string()));
            }
        })
        .with_escape_callback(move || {
            if escape_settled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = escape_sender.send(StartupEvent::Cancel);
            }
        });
    let component = Arc::new(Mutex::new(StartupInputComponent {
        title,
        placeholder: placeholder.map(str::to_string),
        input,
    }));
    let (mut ui, _guard) = setup_startup_tui(component, settings)?;
    let result = match next_startup_event(&mut ui, &receiver)? {
        StartupEvent::Input(value) => Some(value),
        StartupEvent::Confirm | StartupEvent::Cancel => None,
    };
    finish_startup_tui(&mut ui).await?;
    Ok(result)
}

struct StartupInputComponent {
    title: String,
    placeholder: Option<String>,
    input: Input,
}

impl Component for StartupInputComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![border(width, true)];
        lines.extend(text_lines(
            tui_theme::bold(tui_theme::fg("accent", &self.title)),
            width,
        ));
        lines.extend(self.input.render(width.saturating_sub(2)));
        if self.input.value.is_empty() {
            if let Some(placeholder) = &self.placeholder {
                lines.extend(text_lines(tui_theme::fg("muted", placeholder), width));
            }
        }
        lines.extend(text_lines(
            tui_theme::fg("muted", key_hint("confirm")),
            width,
        ));
        lines.push(border(width, false));
        lines
    }

    fn handle_input(&mut self, key: &TuiKey) {
        self.input.handle_input(key);
    }

    fn set_focused(&mut self, focused: bool) {
        self.input.set_focused(focused);
    }
}

/// Show and, when confirmed, persist the official first-run settings.
pub async fn show_first_time_setup(settings: &mut SettingsManager) -> Result<(), String> {
    let detected_theme = crate::theme::default_theme();
    tui_theme::load_theme(&detected_theme);
    let (sender, receiver) = mpsc::channel();
    let settled = Arc::new(AtomicBool::new(false));
    let component = Arc::new(Mutex::new(FirstTimeSetupComponent::new(
        &detected_theme,
        sender.clone(),
    )));
    let (mut ui, _guard) = setup_startup_tui(component.clone(), settings)?;
    let result = loop {
        match next_startup_event(&mut ui, &receiver)? {
            StartupEvent::Input(value) => {
                component
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .preview_theme(&value);
                ui.render_now(true);
            }
            StartupEvent::Confirm => {
                let mut setup = component.lock().unwrap_or_else(|error| error.into_inner());
                if setup.step == SetupStep::Theme {
                    setup.advance_to_analytics(sender.clone());
                    drop(setup);
                    ui.render_now(true);
                } else {
                    if settle_once(&settled) {
                        break Some(setup.result());
                    }
                }
            }
            StartupEvent::Cancel => {
                if settle_once(&settled) {
                    break None;
                }
            }
        }
    };

    let persist_error = if let Some(result) = result {
        settings.set_theme(result.theme);
        settings.set_enable_analytics(result.share_analytics);
        settings.flush().await;
        settings
            .drain_errors()
            .into_iter()
            .next()
            .map(|error| format!("persist first-run settings: {}", error.error))
    } else {
        None
    };
    let terminal_error = finish_startup_tui(&mut ui).await.err();
    if let Some(error) = persist_error.or(terminal_error) {
        return Err(error);
    }
    Ok(())
}

/// Exact runtime gate for the upstream first-run setup.
pub fn should_run_first_time_setup(settings_path: &std::path::Path) -> bool {
    let official_distribution = env!("CARGO_PKG_NAME") == "pi-coding-agent"
        && crate::config::APP_NAME == "pi"
        && crate::config::CONFIG_DIR_NAME == ".pi";
    let uses_default_agent_dir = std::env::var_os(crate::config::ENV_AGENT_DIR).is_none();
    crate::core::experimental::should_run_first_time_setup(
        official_distribution,
        uses_default_agent_dir,
        settings_path.exists(),
    )
}

/// Small test seam for double-settlement callers and embedders.
pub fn settle_once(settled: &AtomicBool) -> bool {
    settled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

#[cfg(test)]
mod interactive_startup_tests {
    use super::*;
    use crate::core::settings::SettingsMap;

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn temp_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-startup-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn startup_presentation_lists_resources_and_toggles_expansion() {
        let cwd = temp_root("resources");
        let agent_dir = cwd.join("agent");
        std::fs::create_dir_all(agent_dir.join("skills/demo")).unwrap();
        std::fs::create_dir_all(cwd.join(".pi/prompts")).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "instructions\n").unwrap();
        std::fs::write(
            agent_dir.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nUse the demo skill.\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".pi/prompts/hello.md"),
            "---\ndescription: Say hello\n---\nHello\n",
        )
        .unwrap();

        let args = Args::default();
        let settings = SettingsManager::in_memory(SettingsMap::new());
        let resources = ResourceDiscovery::default();
        let (prompts, diagnostics) = crate::core::prompt_templates::load_prompt_templates(
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            &[],
            true,
            false,
        );
        assert!(diagnostics.is_empty());
        let presentation = InteractiveStartupPresentation::new(
            "0.84.2",
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            &args,
            &settings,
            &resources,
            &[],
            &[],
            &prompts,
            false,
        );
        let compact = presentation.render(120).join("\n");
        assert!(compact.contains("Pi can explain its own features"));
        assert!(compact.contains("[Context]"));
        assert!(compact.contains("[Skills]"));
        assert!(compact.contains("demo"));
        assert!(compact.contains("[Prompts]"));
        assert!(compact.contains("/hello"));
        let plain = pi_tui::strip_ansi_codes(&compact);
        let plain_lines: Vec<_> = plain.lines().collect();
        let context_line = plain_lines
            .iter()
            .position(|line| line.trim() == "[Context]")
            .expect("context heading is rendered");
        assert!(context_line >= 2);
        assert!(plain_lines[context_line - 1].is_empty());
        assert!(plain_lines[context_line - 2].is_empty());

        let mut expanded_presentation = presentation;
        assert!(!expanded_presentation.is_expanded());
        assert!(expanded_presentation.toggle());
        let expanded = expanded_presentation.render(120).join("\n");
        assert!(pi_tui::strip_ansi_codes(&expanded).contains("ctrl+c twice to exit"));
        assert!(expanded.contains("SKILL.md"));
        assert!(expanded.contains("hello.md"));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn startup_component_handles_ctrl_o() {
        let (collapsed, expanded) = startup_header("test");
        let mut component = InteractiveStartupPresentation {
            header_collapsed: collapsed,
            header_expanded: expanded,
            resources_collapsed: String::new(),
            resources_expanded: String::new(),
            context_leading_spacer: false,
            expanded: false,
        };
        component.handle_input(&pi_tui::keys::parse_key("ctrl+o"));
        assert!(component.is_expanded());
        component.handle_input(&pi_tui::keys::parse_key("ctrl+p"));
        assert!(component.is_expanded());
    }

    #[test]
    fn startup_ui_inherits_persisted_terminal_display_settings() {
        let mut settings = SettingsManager::in_memory(SettingsMap::new());
        settings.set_clear_on_shrink(false);
        settings.set_show_hardware_cursor(true);
        let terminal = Arc::new(Mutex::new(TerminalBackend::new()));
        let mut ui = TuiMainScreen::new(terminal);

        configure_startup_tui(&mut ui, &settings);

        assert!(!ui.get_clear_on_shrink());
        assert!(ui.get_show_hardware_cursor());
    }

    #[test]
    fn startup_borders_never_exceed_the_requested_width() {
        for width in 0..=8 {
            assert!(pi_tui::visible_width(&border(width, true)) <= width);
            assert!(pi_tui::visible_width(&border(width, false)) <= width);
        }
        assert_eq!(border(1, true), "╭");
        assert_eq!(border(1, false), "╰");
    }
}
