//! Interactive TUI mode — port of `packages/coding-agent/src/modes/interactive/
//! interactive-mode.ts` using the ported pi-tui component surface.
//!
//! Drives the Editor (multi-line, history, undo, autocomplete), the Markdown
//! transcript, slash-command dispatch with model/thinking/theme/settings
//! selectors, a footer, and the agent turn loop.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::harness::agent_harness::{AgentLane, HarnessError};
use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::rich_agent::RichAgentEvent;
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::session::Session as JsonlSession;
use pi_agent::session::state::{ForkOptions, ForkPosition};
use pi_agent::session::types::{Entry, EntryNoStats};
use pi_agent::session::JsonlSessionRepo;
use pi_ai::auth::AuthInteraction;
use pi_ai::model::Model;
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Message, ToolResultMessage,
};
use serde_json::{json, Value};

use crate::args::Args;
use crate::config;
use crate::core::extensions::{
    install_tools, load_for_mode, load_for_mode_with_reason_and_flags_and_previous,
    register_loaded_native_providers, ExtensionHostActions, ExtensionHostState, LoadedExtensions,
    ResourceDiscovery,
};
use crate::core::settings::SettingsManager;
use crate::interactive as it;
use crate::interactive::auth::{AuthSurfaceAction, AuthSurfaceState};
use crate::interactive::footer::{self, FooterData, FooterExtras};
use crate::interactive::selectors::ListSelector;
use crate::interactive::settings_panel::SettingsPanel;
use crate::interactive::slash::SlashKind;
use crate::interactive::{Modal, SubmitAction};

use pi_tui::components::select_list::SelectItem;
use pi_tui::components::{Editor, Markdown, ScrollView, Text};
use pi_tui::keys::{is_key_release, parse_key, TuiKey};

use pi_tui::terminal::TerminalBackend;
use pi_tui::tui::{Component, Scene, SharedComponent, Tree};
use pi_tui::TuiAltScreen;
use pi_tui::TuiStopOptions;

type InteractiveEventHandler =
    Arc<Mutex<Option<Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>>>>;
type InteractiveToolEventHandler = Arc<Mutex<Option<Arc<dyn Fn(&RichAgentEvent) + Send + Sync>>>>;

/// Match `TuiBase.MIN_RENDER_INTERVAL_MS` from the pinned upstream renderer.
/// Components still request renders at their own cadence; this is the maximum
/// latency allowed for a live animation or redraw request to reach the TUI.
const TUI_MIN_RENDER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Bound the amount of queued text handled by one owner-loop fast pass. This
/// keeps a continuous paste from starving transcript/animation maintenance,
/// while adjacent input that arrived in the same scheduling window still
/// produces one immediate repaint instead of one frame per scalar character.
const MAX_IMMEDIATE_EDITOR_EVENTS: usize = 64;

/// Renderer used by the interactive mode.
///
/// Regular mode retains the scrollback-oriented `Tree` renderer. Fullscreen
/// mode uses the retained `TuiAltScreen` controller so viewport scrolling,
/// transcript search, mouse selection, scrollbar gestures, overlays, and
/// differential rendering share the same path as the public pi-tui surface.
/// Keeping the two modes explicit preserves Pi's regular/fullscreen setting
/// instead of silently forcing every invocation into an alternate screen.
enum InteractiveRenderer {
    Regular {
        tree: Tree,
    },
    Fullscreen {
        tui: Box<TuiAltScreen>,
        force_render: bool,
        render_wakeup: Arc<tokio::sync::Notify>,
    },
}

impl InteractiveRenderer {
    fn new(terminal: Arc<Mutex<TerminalBackend>>, fullscreen: bool) -> Self {
        if fullscreen {
            let render_wakeup = Arc::new(tokio::sync::Notify::new());
            let render_wakeup_for_callback = Arc::clone(&render_wakeup);
            let mut tui = Box::new(TuiAltScreen::new(terminal));
            tui.set_request_render_callback(Some(Arc::new(move || {
                render_wakeup_for_callback.notify_one();
            })));
            Self::Fullscreen {
                tui,
                force_render: true,
                render_wakeup,
            }
        } else {
            Self::Regular {
                tree: Tree::new(terminal),
            }
        }
    }

    fn terminal_handle(&self) -> Arc<Mutex<TerminalBackend>> {
        match self {
            Self::Regular { tree } => tree.terminal_handle(),
            Self::Fullscreen { tui, .. } => tui.terminal(),
        }
    }

    fn focus(&mut self, component: SharedComponent) {
        match self {
            Self::Regular { tree } => tree.focus(component),
            Self::Fullscreen { tui, .. } => tui.set_focus(Some(component)),
        }
    }

    fn query_cell_size(&mut self) -> bool {
        self.terminal_handle()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .query_cell_size()
    }

    fn consume_cell_size_response(&mut self, data: &str) -> bool {
        let consumed = self
            .terminal_handle()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .consume_cell_size_response(data);
        if consumed {
            self.invalidate();
        }
        consumed
    }

    fn invalidate(&mut self) {
        match self {
            Self::Regular { tree } => tree.invalidate(),
            Self::Fullscreen { force_render, .. } => *force_render = true,
        }
    }

    /// Apply settings that belong to the live TUI controller in either
    /// regular scrollback or fullscreen alt-screen mode.
    fn set_show_hardware_cursor(&mut self, enabled: bool) {
        match self {
            Self::Regular { tree } => tree.set_show_hardware_cursor(enabled),
            Self::Fullscreen { tui, .. } => tui.set_show_hardware_cursor(enabled),
        }
    }

    fn set_clear_on_shrink(&mut self, enabled: bool) {
        match self {
            Self::Regular { tree } => tree.set_clear_on_shrink(enabled),
            Self::Fullscreen { tui, .. } => tui.set_clear_on_shrink(enabled),
        }
    }

    fn is_fullscreen(&self) -> bool {
        matches!(self, Self::Fullscreen { .. })
    }

    /// Switch the live renderer while raw mode remains owned by the
    /// interactive loop. The upstream settings selector applies `tuiMode`
    /// immediately; keeping the terminal handle and rebuilding only the
    /// controller preserves the retained scene, editor focus, and loader
    /// repaint callback across the transition.
    fn switch_mode(
        &mut self,
        fullscreen: bool,
        editor: SharedComponent,
        loader: &mut pi_tui::components::Loader,
        show_hardware_cursor: bool,
        clear_on_shrink: bool,
    ) -> bool {
        if self.is_fullscreen() == fullscreen {
            return false;
        }

        let terminal = self.terminal_handle();
        if fullscreen {
            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .enter_alt_screen();
        } else {
            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .leave_alt_screen();
        }
        *self = Self::new(terminal, fullscreen);
        self.set_show_hardware_cursor(show_hardware_cursor);
        self.set_clear_on_shrink(clear_on_shrink);
        self.attach_loader_repaint(loader);
        self.focus(editor);
        self.invalidate();
        true
    }

    fn render_scene(&mut self, scene: &Arc<Mutex<Scene>>) {
        match self {
            Self::Regular { tree } => tree.render(Some(scene)),
            Self::Fullscreen {
                tui, force_render, ..
            } => {
                let root: SharedComponent = scene.clone();
                tui.set_layout_root(Some(root));
                tui.render_now(*force_render);
                *force_render = false;
            }
        }
    }

    fn has_pending_render_request(&self) -> bool {
        match self {
            Self::Regular { .. } => false,
            Self::Fullscreen {
                tui, force_render, ..
            } => *force_render || tui.is_render_requested(),
        }
    }

    /// Repaint the last complete scene immediately after latency-sensitive
    /// editor input.  The interactive owner still prepares the authoritative
    /// transcript/footer/status scene on its next iteration; this fast path
    /// only reuses that scene so the shared editor component can show the new
    /// draft without waiting behind that preparation work.  This mirrors
    /// Pi's focused-input -> requestImmediateRender path.
    fn render_cached_scene(&mut self, scene: Option<&Arc<Mutex<Scene>>>) -> bool {
        let Some(scene) = scene else {
            return false;
        };
        self.render_scene(scene);
        true
    }

    /// Give fullscreen-only viewport gestures first refusal before the
    /// application handles editor and slash-command input.
    fn dispatch_viewport_input(&mut self, raw: &str) -> bool {
        match self {
            Self::Regular { .. } => false,
            Self::Fullscreen { tui, .. } => tui.dispatch_viewport_input(raw),
        }
    }

    /// Stop the renderer with Pi's fullscreen exit policy. Transcript mode
    /// projects the retained scene through the regular scrollback renderer
    /// before raw/alternate-screen teardown; resume-hint mode exits without
    /// replaying the fullscreen document.
    fn stop(&mut self, fullscreen_exit_output: &str, transcript: Option<&Arc<Mutex<Scene>>>) {
        match self {
            Self::Regular { tree } => tree.leave_alt_screen(),
            Self::Fullscreen { tui, .. } => {
                let show_transcript = fullscreen_exit_output == "transcript";
                let terminal = tui.terminal();
                let _ = tui.stop(TuiStopOptions {
                    preserve_screen: true,
                });

                if show_transcript {
                    // run_interactive_mode entered the terminal before
                    // constructing TuiAltScreen, so its controller does not
                    // own the `started` flag. Leave only the alternate screen
                    // here and retain raw mode while Tree projects the scene.
                    terminal
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .leave_alt_screen();
                    let mut tree = Tree::new(terminal.clone());
                    if let Some(scene) = transcript {
                        tree.render(Some(scene));
                    }
                    // Match TuiMainScreen.beforeTerminalStop: leave the
                    // projected transcript above the shell prompt.
                    terminal
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .write_raw("\r\n");
                }

                let _ = terminal
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .leave_raw();
            }
        }
    }

    fn attach_loader_repaint(&self, loader: &mut pi_tui::components::Loader) {
        if let Self::Fullscreen { tui, .. } = self {
            loader.set_request_render_callback(Some(tui.request_render_callback()));
        }
    }

    fn render_wakeup(&self) -> Option<Arc<tokio::sync::Notify>> {
        match self {
            Self::Regular { .. } => None,
            Self::Fullscreen { render_wakeup, .. } => Some(Arc::clone(render_wakeup)),
        }
    }
}

/// Resolve the scrollbar mode for the currently mounted TUI. The setting is
/// intentionally inert in regular mode, but must be applied when a live
/// settings change switches the existing transcript view into fullscreen.
fn fullscreen_scrollbar_mode(setting: &str, fullscreen: bool) -> pi_tui::ScrollbarMode {
    if !fullscreen {
        return pi_tui::ScrollbarMode::Hidden;
    }
    match setting {
        "always" => pi_tui::ScrollbarMode::Always,
        "hidden" => pi_tui::ScrollbarMode::Hidden,
        _ => pi_tui::ScrollbarMode::Auto,
    }
}

fn modal_identity(modal: Option<&Modal>) -> Option<usize> {
    let component = match modal? {
        Modal::Model(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Llama(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::HuggingFace(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::HuggingFaceDownload(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::ScopedModels(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Thinking(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Theme(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Fork(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Settings(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Tree(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::Trust(component) => Arc::as_ptr(component) as *const () as usize,
        Modal::LlamaLoadPlan { selector, .. } => Arc::as_ptr(selector) as *const () as usize,
        Modal::LlamaUnloadConfirm { selector, .. } => Arc::as_ptr(selector) as *const () as usize,
        Modal::Resume(selector, _) => Arc::as_ptr(selector) as *const () as usize,
        Modal::CrossProjectSession(prompt) => Arc::as_ptr(prompt) as *const () as usize,
    };
    Some(component)
}

async fn wait_for_render_wakeup(wakeup: Option<Arc<tokio::sync::Notify>>) {
    match wakeup {
        Some(notify) => notify.notified().await,
        None => std::future::pending::<()>().await,
    }
}

/// Portable owner-loop wrapper for the Unix shutdown signals. `tokio::select!`
/// does not accept `cfg` attributes inside its branch list, so keep the
/// platform-specific signal receivers behind this helper and expose one
/// ordinary future to the main interactive select.
struct InteractiveShutdownSignals {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sighup: tokio::signal::unix::Signal,
}

impl InteractiveShutdownSignals {
    fn new() -> Result<Self, String> {
        #[cfg(unix)]
        {
            let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|error| format!("watch SIGTERM: {error}"))?;
            let sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .map_err(|error| format!("watch SIGHUP: {error}"))?;
            Ok(Self { sigterm, sighup })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn recv(&mut self) -> Option<()> {
        #[cfg(unix)]
        {
            tokio::select! {
                signal = self.sigterm.recv() => signal.map(|_| ()),
                signal = self.sighup.recv() => signal.map(|_| ()),
            }
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<Option<()>>().await
        }
    }
}

/// A status line that has been committed to the interactive transcript.
///
/// `message_index` is the number of persisted agent messages that existed
/// when Pi added the status to `chatContainer`. Keeping that boundary lets
/// the Rust renderer rebuild the retained document after a new turn without
/// moving an old status below a later prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveStatusEntry {
    pub message_index: usize,
    pub message: String,
}

/// The interactive status lifecycle shared by the fullscreen and regular
/// render paths.
///
/// Ordinary `showStatus` output is transcript content. Only an active status
/// (working/retry/compaction or another operation that is still running) is
/// exposed through [`active_message`] for the lower dock. Back-to-back status
/// messages at the same transcript boundary replace one another, matching
/// Pi's `lastStatusSpacer`/`lastStatusText` behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractiveStatusLog {
    entries: Vec<InteractiveStatusEntry>,
    active: Option<String>,
    revision: u64,
    last_status_anchor: Option<usize>,
    last_status_replaceable: bool,
    /// Index at which statuses emitted after a persistent hidden component
    /// begin. Pi appends those statuses after the component in the chat
    /// container; keeping the split lets the Rust projection preserve that
    /// order even though the ordinary transcript is indexed by messages.
    tail_start: Option<usize>,
}

impl InteractiveStatusLog {
    /// Add an ordinary status at a precise transcript boundary.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn show_status(&mut self, message: impl Into<String>, message_index: usize) {
        let message = message.into();
        if message.trim().is_empty() {
            return;
        }

        // The render loop observes the scalar banner on every frame. Once a
        // hidden component has established a document boundary, an unchanged
        // banner must not be appended repeatedly after that boundary.
        if !self.last_status_replaceable
            && self.last_status_anchor == Some(message_index)
            && self.entries.last().is_some_and(|entry| {
                entry.message_index == message_index && entry.message == message
            })
        {
            if self.active.take().is_some() {
                self.revision = self.revision.wrapping_add(1);
            }
            return;
        }

        let mut changed = true;
        if self.last_status_replaceable
            && self.last_status_anchor == Some(message_index)
            && self
                .entries
                .last()
                .is_some_and(|entry| entry.message_index == message_index)
        {
            let entry = self
                .entries
                .last_mut()
                .expect("last status exists when replacement is allowed");
            changed = entry.message != message;
            entry.message = message;
        } else {
            self.entries.push(InteractiveStatusEntry {
                message_index,
                message,
            });
        }
        if self.active.take().is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        self.last_status_anchor = Some(message_index);
        self.last_status_replaceable = true;
    }

    /// End the current chat-child insertion point after a persistent hidden
    /// component. The next changed status is appended after that component;
    /// an unchanged scalar banner remains replaceable/no-op at its existing
    /// position until a new status is observed.
    fn mark_hidden_component_boundary(&mut self) {
        if self.tail_start.is_none() {
            self.tail_start = Some(self.entries.len());
            self.revision = self.revision.wrapping_add(1);
        }
        self.last_status_replaceable = false;
    }

    /// Observe the mode's legacy status slot and route it to the correct
    /// surface. The mode still uses a scalar slot while command handlers are
    /// being migrated; observing it once per render preserves all status
    /// ordering at the actual turn boundary.
    pub fn observe_banner(&mut self, message: &str, message_index: usize, active: bool) {
        if active {
            let next_active = (!message.trim().is_empty()).then(|| message.to_string());
            if self.active != next_active {
                self.revision = self.revision.wrapping_add(1);
            }
            self.active = next_active;
            // Starting active work is a content boundary. A later terminal
            // status must not replace the last ordinary line merely because
            // the provider returned no persisted messages (for example an
            // early abort).
            self.last_status_anchor = None;
            self.last_status_replaceable = false;
            return;
        }

        if self.active.take().is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        if !message.trim().is_empty() {
            self.show_status(message.to_string(), message_index);
        }
    }

    /// Mark the beginning of an active turn whose final status is not known
    /// yet. This is used before the provider worker starts, so an abort/error
    /// cannot overwrite a preceding status at the same message count.
    pub fn begin_active(&mut self) {
        if self.active.take().is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.last_status_anchor = None;
        self.last_status_replaceable = false;
    }

    /// Remove all transcript and dock status state when the session view is
    /// replaced or cleared.
    pub fn clear(&mut self) {
        if !self.entries.is_empty() || self.active.is_some() || self.tail_start.is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.entries.clear();
        self.active = None;
        self.last_status_anchor = None;
        self.last_status_replaceable = false;
        self.tail_start = None;
    }

    pub fn entries(&self) -> &[InteractiveStatusEntry] {
        &self.entries
    }

    fn transcript_entries(&self) -> &[InteractiveStatusEntry] {
        let end = self.tail_start.unwrap_or(self.entries.len());
        &self.entries[..end]
    }

    fn tail_entries(&self) -> &[InteractiveStatusEntry] {
        let start = self.tail_start.unwrap_or(self.entries.len());
        &self.entries[start..]
    }

    pub fn active_message(&self) -> Option<&str> {
        self.active.as_deref()
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

/// Ordered document blocks used to materialize Pi's separate chat children
/// without putting ordinary statuses into the fixed dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveTranscriptBlock {
    Markdown(String),
    /// A user prompt is its own upstream component. Keeping it separate from
    /// the assistant markdown prevents a synthetic `### You` heading and
    /// lets the view apply the user-message style/boundary independently.
    UserMessage(String),
    /// Built-in tool output is rendered by the tool component boundary rather
    /// than being folded into the ordinary assistant markdown stream.
    ToolExecution(String),
    BashExecution(String),
    Spacer,
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractiveTranscriptRenderKey {
    message_count: usize,
    cache_entry_count: usize,
    stream: String,
    status_revision: u64,
    active_status: Option<String>,
    options: it::messages::TranscriptRenderOptions,
    show_cache_miss_notices: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractiveDocumentShape {
    has_status_tail: bool,
    easter_egg_ids: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractiveSceneShape {
    modal_id: Option<usize>,
    pending_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractiveFooterRenderKey {
    message_count: usize,
    cache_entry_count: usize,
    session_name: Option<String>,
    provider: String,
    model_id: String,
    model_label: String,
    thinking: String,
    reasoning: bool,
    context_window: u64,
    auto_compact: bool,
    terminal_width: usize,
    modal_id: Option<usize>,
    using_subscription: bool,
    extension_statuses: Vec<(String, String)>,
    invalidation_generation: u64,
}

fn append_message_blocks(
    output: &mut Vec<InteractiveTranscriptBlock>,
    messages: &[pi_agent::types::AgentMessage],
    base_index: usize,
    options: it::messages::TranscriptRenderOptions,
    cache_notices: &[(u64, String)],
) {
    for (relative_index, message) in messages.iter().enumerate() {
        if let Some((kind, text)) = it::messages::render_message_with_options(message, options) {
            match kind.as_str() {
                "user" => output.push(InteractiveTranscriptBlock::UserMessage(text)),
                "tool" => output.push(InteractiveTranscriptBlock::ToolExecution(text)),
                "bash" => output.push(InteractiveTranscriptBlock::BashExecution(text)),
                "assistant" | "banner" => output.push(InteractiveTranscriptBlock::Markdown(text)),
                _ => {}
            }

            if let pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) = message {
                if let Some((_, notice)) = cache_notices
                    .iter()
                    .find(|(timestamp, _)| *timestamp == assistant.timestamp())
                {
                    output.push(InteractiveTranscriptBlock::Text(it::tui_theme::fg(
                        "dim",
                        format!("> {notice}"),
                    )));
                }
            }
        }
        let _ = base_index + relative_index;
    }
}

fn render_message_range(
    messages: &[pi_agent::types::AgentMessage],
    start: usize,
    end: usize,
    options: it::messages::TranscriptRenderOptions,
    cache_notices: &[(u64, String)],
) -> Vec<InteractiveTranscriptBlock> {
    let mut output = Vec::new();
    append_message_blocks(
        &mut output,
        &messages[start..end],
        start,
        options,
        cache_notices,
    );
    output
}

/// Build the ordered chat projection used by the live interactive mode.
///
/// The returned block order is directly observable: each ordinary status is
/// represented by `Spacer, Text` inside the scrollable document, while active
/// status text is deliberately absent and is supplied by the dock caller.
pub fn build_interactive_transcript_blocks(
    messages: &[pi_agent::types::AgentMessage],
    options: it::messages::TranscriptRenderOptions,
    stream_text: &str,
    cache_notices: &[(u64, String)],
    statuses: &InteractiveStatusLog,
) -> Vec<InteractiveTranscriptBlock> {
    let mut blocks = Vec::new();
    let mut message_start = 0;
    let mut status_cursor = 0;
    let status_entries = statuses.transcript_entries();

    for message_index in 0..=messages.len() {
        if message_index > message_start {
            let rendered = render_message_range(
                messages,
                message_start,
                message_index,
                options,
                cache_notices,
            );
            blocks.extend(rendered);
            message_start = message_index;
        }

        let status_start = status_cursor;
        while status_cursor < status_entries.len()
            && status_entries[status_cursor].message_index == message_index
        {
            status_cursor += 1;
        }
        if status_cursor == status_start {
            continue;
        }

        for entry in &status_entries[status_start..status_cursor] {
            blocks.push(InteractiveTranscriptBlock::Spacer);
            blocks.push(InteractiveTranscriptBlock::Text(it::tui_theme::fg(
                "dim",
                &entry.message,
            )));
        }
        if message_index < messages.len() || !stream_text.trim().is_empty() {
            // A subsequent chat child causes Pi's addMessageToChat path to
            // insert its own spacer after the status text.
            blocks.push(InteractiveTranscriptBlock::Spacer);
        }
    }

    if message_start < messages.len() {
        let rendered = render_message_range(
            messages,
            message_start,
            messages.len(),
            options,
            cache_notices,
        );
        blocks.extend(rendered);
    }

    let stream = stream_text.trim_start();
    if !stream.trim().is_empty() {
        let stream = format!("▌ {stream}");
        let stream_block = if stream.contains("**$ ") {
            InteractiveTranscriptBlock::BashExecution(stream)
        } else {
            InteractiveTranscriptBlock::Markdown(stream)
        };
        if let Some(InteractiveTranscriptBlock::Markdown(last)) = blocks.last_mut() {
            if !last.is_empty() {
                last.push('\n');
            }
            match stream_block {
                InteractiveTranscriptBlock::Markdown(stream) => last.push_str(&stream),
                other => blocks.push(other),
            }
        } else {
            blocks.push(stream_block);
        }
    }

    blocks
}

fn build_interactive_status_tail_blocks(
    statuses: &InteractiveStatusLog,
) -> Vec<InteractiveTranscriptBlock> {
    let mut blocks = Vec::new();
    for entry in statuses.tail_entries() {
        blocks.push(InteractiveTranscriptBlock::Spacer);
        blocks.push(InteractiveTranscriptBlock::Text(it::tui_theme::fg(
            "dim",
            &entry.message,
        )));
    }
    blocks
}

fn transcript_source_from_blocks(blocks: &[InteractiveTranscriptBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            InteractiveTranscriptBlock::Markdown(text) => output.push_str(text),
            InteractiveTranscriptBlock::UserMessage(text)
            | InteractiveTranscriptBlock::ToolExecution(text)
            | InteractiveTranscriptBlock::BashExecution(text) => output.push_str(text),
            InteractiveTranscriptBlock::Spacer => output.push('\n'),
            InteractiveTranscriptBlock::Text(text) => output.push_str(text),
        }
    }
    output
}

/// Materialize the upstream per-message component boundary inside the Rust
/// retained transcript. Markdown remains the source for prose, while user
/// and tool blocks get their own styling and built-in bash gets the dynamic
/// horizontal execution frame used by Pi.
struct TranscriptVisualComponent {
    kind: TranscriptVisualKind,
    markdown: Markdown,
}

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END: &str = "\x1b]133;B\x07";
const OSC133_ZONE_FINAL: &str = "\x1b]133;C\x07";

#[derive(Clone, Copy)]
enum TranscriptVisualKind {
    User,
    Tool,
    Bash,
}

impl TranscriptVisualComponent {
    fn new(kind: TranscriptVisualKind, text: String, output_pad: usize) -> Self {
        let default_style =
            matches!(kind, TranscriptVisualKind::User).then(it::tui_theme::user_message_style);
        Self {
            kind,
            markdown: Markdown::new(
                text,
                output_pad.min(1),
                0,
                it::tui_theme::markdown_theme(),
                default_style,
                None,
            ),
        }
    }

    fn paint_background(lines: Vec<String>, width: usize, color: &str) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| it::tui_theme::bg(color, line))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|line| {
                let visible = pi_tui::utils::visible_width(&line);
                if visible < width {
                    format!("{line}{}", " ".repeat(width - visible))
                } else {
                    line
                }
            })
            .collect()
    }

    fn paint_background_preserving_osc133(
        lines: Vec<String>,
        width: usize,
        color: &str,
    ) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                let (prefix, content) = Self::split_osc133_prefix(&line);
                let painted = it::tui_theme::bg(color, content);
                let visible = pi_tui::utils::visible_width(&painted);
                let padded = if visible < width {
                    format!("{painted}{}", " ".repeat(width - visible))
                } else {
                    painted
                };
                format!("{prefix}{padded}")
            })
            .collect()
    }

    fn split_osc133_prefix(line: &str) -> (String, &str) {
        let mut prefix_len = 0;
        for marker in [OSC133_ZONE_START, OSC133_ZONE_END, OSC133_ZONE_FINAL] {
            if line[prefix_len..].starts_with(marker) {
                prefix_len += marker.len();
            } else {
                break;
            }
        }
        (line[..prefix_len].to_string(), &line[prefix_len..])
    }

    fn set_text(&mut self, text: impl Into<String>) {
        self.markdown.set_text(text);
    }
}

impl Component for TranscriptVisualComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let lines = self.markdown.render(width);
        match self.kind {
            TranscriptVisualKind::User => {
                let mut marked_lines = lines;
                match marked_lines.as_mut_slice() {
                    [] => {}
                    [line] => {
                        // Rust Markdown keeps a soft-wrapped paragraph on
                        // one physical row. Preserve the complete OSC133
                        // boundary instead of letting the end assignment
                        // overwrite the start marker.
                        *line = format!(
                            "{OSC133_ZONE_START}{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{line}"
                        );
                    }
                    [first, .., last] => {
                        *first = format!("{OSC133_ZONE_START}{first}");
                        *last = format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{last}");
                    }
                }
                Self::paint_background_preserving_osc133(marked_lines, width, "userMessageBg")
            }
            TranscriptVisualKind::Tool => {
                let color = if self.markdown.text().trim_start().starts_with('✗') {
                    "toolErrorBg"
                } else if self.markdown.text().trim_start().starts_with('⏳') {
                    "toolPendingBg"
                } else {
                    "toolSuccessBg"
                };
                Self::paint_background(lines, width, color)
            }
            TranscriptVisualKind::Bash => {
                let border = it::tui_theme::fg("bashMode", "─".repeat(width.max(1)));
                let mut rendered = vec![String::new(), border.clone()];
                rendered.extend(lines);
                rendered.push(border);
                rendered
            }
        }
    }

    fn invalidate(&mut self) {
        self.markdown.invalidate();
    }
}

/// Retained chat component for the scrollable document. It is rebuilt only
/// when the ordered block projection changes, preserving Markdown caches and
/// keeping keystrokes out of an unconditional full transcript reconstruction.
struct InteractiveTranscriptView {
    blocks: Vec<InteractiveTranscriptBlock>,
    children: Vec<InteractiveTranscriptChild>,
    mermaid_mode: Arc<Mutex<String>>,
    mermaid_streaming: Arc<AtomicBool>,
    output_pad: usize,
    theme_dirty: bool,
    rendered: Mutex<Option<(usize, Vec<String>)>>,
}

/// Typed retained children let a streaming markdown block update in place.
/// Rebuilding every trait object for every assistant delta made a large PTY
/// burst monopolize the event loop and delayed steering/follow-up keys.
enum InteractiveTranscriptChild {
    Markdown(Arc<Mutex<Markdown>>),
    Visual(Arc<Mutex<TranscriptVisualComponent>>),
    Spacer(Arc<Mutex<pi_tui::components::Spacer>>),
    Text(Arc<Mutex<Text>>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InteractiveTranscriptChildKind {
    Markdown,
    User,
    Tool,
    Bash,
    Spacer,
    Text,
}

impl InteractiveTranscriptChild {
    fn kind(&self) -> InteractiveTranscriptChildKind {
        match self {
            Self::Markdown(_) => InteractiveTranscriptChildKind::Markdown,
            Self::Visual(component) => match component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .kind
            {
                TranscriptVisualKind::User => InteractiveTranscriptChildKind::User,
                TranscriptVisualKind::Tool => InteractiveTranscriptChildKind::Tool,
                TranscriptVisualKind::Bash => InteractiveTranscriptChildKind::Bash,
            },
            Self::Spacer(_) => InteractiveTranscriptChildKind::Spacer,
            Self::Text(_) => InteractiveTranscriptChildKind::Text,
        }
    }

    fn set_text(&mut self, text: String) {
        match self {
            Self::Markdown(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_text(text),
            Self::Visual(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_text(text),
            Self::Text(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_text(text),
            Self::Spacer(_) => {}
        }
    }

    fn render(&self, width: usize) -> Vec<String> {
        match self {
            Self::Markdown(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .render(width),
            Self::Visual(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .render(width),
            Self::Spacer(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .render(width),
            Self::Text(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .render(width),
        }
    }

    fn invalidate(&self) {
        match self {
            Self::Markdown(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate(),
            Self::Visual(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate(),
            Self::Spacer(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate(),
            Self::Text(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate(),
        }
    }

    fn set_focused(&self, focused: bool) {
        match self {
            Self::Markdown(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_focused(focused),
            Self::Visual(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_focused(focused),
            Self::Spacer(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_focused(focused),
            Self::Text(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_focused(focused),
        }
    }

    fn set_height(&self, height: usize) {
        match self {
            Self::Markdown(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_height(height),
            Self::Visual(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_height(height),
            Self::Spacer(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_height(height),
            Self::Text(component) => component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_height(height),
        }
    }
}

impl InteractiveTranscriptView {
    fn new(mermaid_mode: Arc<Mutex<String>>, mermaid_streaming: Arc<AtomicBool>) -> Self {
        Self {
            blocks: Vec::new(),
            children: Vec::new(),
            mermaid_mode,
            mermaid_streaming,
            output_pad: 1,
            theme_dirty: false,
            rendered: Mutex::new(None),
        }
    }

    fn set_output_pad(&mut self, output_pad: usize) {
        let output_pad = output_pad.min(1);
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            // Existing Markdown/Text children retain their constructor
            // padding. Rebuild them on the next block projection so changing
            // `/settings` updates already-rendered transcript rows too.
            self.theme_dirty = true;
            *self
                .rendered
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
    }

    fn invalidate_theme(&mut self) {
        self.theme_dirty = true;
        *self
            .rendered
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        for child in &self.children {
            child.invalidate();
        }
    }

    fn child_kind(block: &InteractiveTranscriptBlock) -> InteractiveTranscriptChildKind {
        match block {
            InteractiveTranscriptBlock::Markdown(text) if text.contains("**$ ") => {
                InteractiveTranscriptChildKind::Bash
            }
            InteractiveTranscriptBlock::Markdown(_) => InteractiveTranscriptChildKind::Markdown,
            InteractiveTranscriptBlock::UserMessage(_) => InteractiveTranscriptChildKind::User,
            InteractiveTranscriptBlock::ToolExecution(_) => InteractiveTranscriptChildKind::Tool,
            InteractiveTranscriptBlock::BashExecution(_) => InteractiveTranscriptChildKind::Bash,
            InteractiveTranscriptBlock::Spacer => InteractiveTranscriptChildKind::Spacer,
            InteractiveTranscriptBlock::Text(_) => InteractiveTranscriptChildKind::Text,
        }
    }

    fn build_child(&self, block: &InteractiveTranscriptBlock) -> InteractiveTranscriptChild {
        match block {
            InteractiveTranscriptBlock::Markdown(text) if text.contains("**$ ") => {
                InteractiveTranscriptChild::Visual(Arc::new(Mutex::new(
                    TranscriptVisualComponent::new(
                        TranscriptVisualKind::Bash,
                        text.clone(),
                        self.output_pad,
                    ),
                )))
            }
            InteractiveTranscriptBlock::Markdown(text) => {
                let mermaid_mode = Arc::clone(&self.mermaid_mode);
                let mermaid_streaming = Arc::clone(&self.mermaid_streaming);
                let options = pi_tui::components::markdown::MarkdownOptions {
                    transform: Some(Box::new(move |markdown, width| {
                        let mode = mermaid_mode
                            .lock()
                            .map(|mode| mode.clone())
                            .unwrap_or_else(|_| "off".to_string());
                        it::mermaid::transform_markdown_with_context(
                            markdown,
                            width,
                            &mode,
                            mermaid_streaming.load(Ordering::Acquire),
                            "assistant",
                        )
                    })),
                    ..Default::default()
                };
                InteractiveTranscriptChild::Markdown(Arc::new(Mutex::new(Markdown::new(
                    text.clone(),
                    self.output_pad,
                    0,
                    it::tui_theme::markdown_theme(),
                    None,
                    Some(options),
                ))))
            }
            InteractiveTranscriptBlock::UserMessage(text) => InteractiveTranscriptChild::Visual(
                Arc::new(Mutex::new(TranscriptVisualComponent::new(
                    TranscriptVisualKind::User,
                    text.clone(),
                    self.output_pad,
                ))),
            ),
            InteractiveTranscriptBlock::ToolExecution(text) => InteractiveTranscriptChild::Visual(
                Arc::new(Mutex::new(TranscriptVisualComponent::new(
                    TranscriptVisualKind::Tool,
                    text.clone(),
                    self.output_pad,
                ))),
            ),
            InteractiveTranscriptBlock::BashExecution(text) => InteractiveTranscriptChild::Visual(
                Arc::new(Mutex::new(TranscriptVisualComponent::new(
                    TranscriptVisualKind::Bash,
                    text.clone(),
                    self.output_pad,
                ))),
            ),
            InteractiveTranscriptBlock::Spacer => InteractiveTranscriptChild::Spacer(Arc::new(
                Mutex::new(pi_tui::components::Spacer::new(1)),
            )),
            InteractiveTranscriptBlock::Text(text) => InteractiveTranscriptChild::Text(Arc::new(
                Mutex::new(Text::new(text.clone(), 0, 0, None)),
            )),
        }
    }

    fn set_blocks(&mut self, blocks: Vec<InteractiveTranscriptBlock>) {
        if !self.theme_dirty && self.blocks == blocks {
            return;
        }
        let can_update_in_place = !self.theme_dirty
            && self.children.len() == blocks.len()
            && self
                .children
                .iter()
                .zip(&blocks)
                .all(|(child, block)| child.kind() == Self::child_kind(block));

        if can_update_in_place {
            for (child, block) in self.children.iter_mut().zip(&blocks) {
                let text = match block {
                    InteractiveTranscriptBlock::Markdown(text)
                    | InteractiveTranscriptBlock::UserMessage(text)
                    | InteractiveTranscriptBlock::ToolExecution(text)
                    | InteractiveTranscriptBlock::BashExecution(text)
                    | InteractiveTranscriptBlock::Text(text) => Some(text.clone()),
                    InteractiveTranscriptBlock::Spacer => None,
                };
                if let Some(text) = text {
                    child.set_text(text);
                }
            }
        } else {
            self.children = blocks.iter().map(|block| self.build_child(block)).collect();
        }
        self.blocks = blocks;
        self.theme_dirty = false;
        *self
            .rendered
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

impl Component for InteractiveTranscriptView {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = self
            .rendered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            if *cached_width == width {
                return lines.clone();
            }
        }
        let lines = self
            .children
            .iter()
            .flat_map(|child| child.render(width))
            .collect::<Vec<_>>();
        *self
            .rendered
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((width, lines.clone()));
        lines
    }

    fn invalidate(&mut self) {
        *self
            .rendered
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        for child in &self.children {
            child.invalidate();
        }
    }

    fn set_focused(&mut self, focused: bool) {
        for child in &self.children {
            child.set_focused(focused);
        }
    }

    fn set_height(&mut self, height: usize) {
        for child in &self.children {
            child.set_height(height);
        }
    }
}

/// One tool execution as it appears in the live interactive transcript.
///
/// The agent emits assistant message events and tool lifecycle events on the
/// same subscription. Keeping this small projection separate from the
/// persisted message list lets the TUI show the call, partial output, and
/// final result immediately, just like Pi's ToolExecutionComponent, without
/// exposing the model-facing JSON envelope.
struct InteractiveLiveTool {
    tool_call_id: String,
    tool_name: String,
    args: Value,
    partial_result: Option<Value>,
    final_result: Option<(pi_agent::tools::AgentToolResult, bool)>,
    started_at: std::time::Instant,
    elapsed: Option<std::time::Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractiveLiveSegment {
    Markdown(String),
    ToolCall(String),
}

#[derive(Default)]
struct InteractiveLiveTranscript {
    history: Vec<String>,
    segments: Vec<InteractiveLiveSegment>,
    tools: Vec<InteractiveLiveTool>,
    options: it::messages::TranscriptRenderOptions,
}

impl InteractiveLiveTranscript {
    fn configure(&mut self, options: it::messages::TranscriptRenderOptions) {
        self.options = options;
    }

    fn clear(&mut self) {
        self.history.clear();
        self.segments.clear();
        self.tools.clear();
    }

    fn on_assistant_event(&mut self, event: &AssistantMessageEvent, _rendered: Option<String>) {
        if matches!(event, AssistantMessageEvent::Start { .. }) {
            self.commit_current();
        }

        let segments =
            it::messages::render_assistant_event_segments_with_options(event, self.options)
                .into_iter()
                .map(|segment| match segment {
                    it::messages::AssistantTranscriptSegment::Markdown(text) => {
                        InteractiveLiveSegment::Markdown(text)
                    }
                    it::messages::AssistantTranscriptSegment::ToolCall(id) => {
                        InteractiveLiveSegment::ToolCall(id)
                    }
                })
                .collect::<Vec<_>>();

        // Once a tool has completed, a provider may emit the next assistant
        // message without another explicit Start event. Preserve the Pi
        // child order by committing the previous tool-bearing projection
        // before accepting that new prose snapshot.
        let contains_current_tool = segments.iter().any(|segment| {
            matches!(segment, InteractiveLiveSegment::ToolCall(id)
                if self.tools.iter().any(|tool| tool.tool_call_id == id.as_str()))
        });
        let contains_tool_call = segments
            .iter()
            .any(|segment| matches!(segment, InteractiveLiveSegment::ToolCall(_)));
        if !self.tools.is_empty() && !contains_current_tool && !contains_tool_call {
            self.commit_current();
        }
        self.segments = segments;
    }

    fn on_tool_event(&mut self, event: &RichAgentEvent) {
        match event {
            RichAgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.ensure_tool(tool_call_id, tool_name, args.clone());
            }
            RichAgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => {
                let index = self.ensure_tool(tool_call_id, tool_name, args.clone());
                self.tools[index].partial_result = Some(partial_result.clone());
            }
            RichAgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let index = self.ensure_tool(tool_call_id, tool_name, Value::Null);
                self.tools[index].final_result = Some((result.clone(), *is_error));
                self.tools[index].elapsed = Some(self.tools[index].started_at.elapsed());
            }
            _ => {}
        }
    }

    fn ensure_tool(&mut self, tool_call_id: &str, tool_name: &str, args: Value) -> usize {
        if let Some(index) = self
            .tools
            .iter()
            .position(|tool| tool.tool_call_id == tool_call_id)
        {
            if !args.is_null() {
                self.tools[index].args = args;
            }
            return index;
        }

        self.tools.push(InteractiveLiveTool {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            args,
            partial_result: None,
            final_result: None,
            started_at: std::time::Instant::now(),
            elapsed: None,
        });
        self.tools.len() - 1
    }

    fn commit_current(&mut self) {
        let rendered = self.render_current();
        if !rendered.trim().is_empty() {
            self.history.push(rendered);
        }
        self.segments.clear();
        self.tools.clear();
    }

    fn render_tool(&self, tool: &InteractiveLiveTool) -> String {
        if tool.tool_name == "bash" {
            render_live_bash_tool(tool, self.options)
        } else {
            let mut rendered = render_live_tool_call(&tool.tool_name, &tool.args, self.options);
            if let Some((result, is_error)) = &tool.final_result {
                rendered.push_str("\n\n");
                rendered.push_str(&render_live_tool_result(
                    &tool.tool_call_id,
                    &tool.tool_name,
                    result,
                    *is_error,
                    self.options,
                ));
            } else if let Some(partial_result) = &tool.partial_result {
                if let Some(partial) = render_live_partial_tool_result(
                    &tool.tool_call_id,
                    &tool.tool_name,
                    partial_result,
                    self.options,
                ) {
                    rendered.push_str("\n\n");
                    rendered.push_str(&partial);
                }
            }
            rendered
        }
    }

    fn render_current(&self) -> String {
        let mut parts = Vec::new();
        let mut referenced_tools = HashSet::new();
        for segment in &self.segments {
            match segment {
                InteractiveLiveSegment::Markdown(text) if !text.trim().is_empty() => {
                    parts.push(text.clone())
                }
                InteractiveLiveSegment::ToolCall(tool_call_id) => {
                    if let Some(tool) = self
                        .tools
                        .iter()
                        .find(|tool| tool.tool_call_id == *tool_call_id)
                    {
                        referenced_tools.insert(tool_call_id.clone());
                        parts.push(self.render_tool(tool));
                    }
                }
                InteractiveLiveSegment::Markdown(_) => {}
            }
        }

        // A tool-start event can race the provider's final tool-call snapshot.
        // Keep it visible at the end instead of dropping a real lifecycle
        // event merely because its assistant segment has not arrived yet.
        for tool in &self.tools {
            if !referenced_tools.contains(&tool.tool_call_id) {
                parts.push(self.render_tool(tool));
            }
        }
        parts.join("\n\n")
    }

    fn render(&self) -> String {
        let mut parts = self.history.clone();
        let current = self.render_current();
        if !current.trim().is_empty() {
            parts.push(current);
        }
        parts.join("\n\n")
    }
}

/// Render the built-in bash lifecycle as one retained execution block. Pi's
/// bash renderer keeps the `$ command` call header, streams output beneath it,
/// and appends the measured `Took` line when the execution settles; it does
/// not replace that block with the generic `✓ bash` tool-result label.
fn render_live_bash_tool(
    tool: &InteractiveLiveTool,
    options: it::messages::TranscriptRenderOptions,
) -> String {
    let command = tool
        .args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("...");
    let mut rendered = format!("**$ {}**", it::messages::escape_display_text(command));
    if let Some(partial) = &tool.partial_result {
        let output = partial
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let output = output.trim();
        if !output.is_empty() {
            rendered.push_str("\n\n");
            rendered.push_str(&it::messages::escape_display_text(output));
        }
    }
    if let Some((result, is_error)) = &tool.final_result {
        rendered = format!("**$ {}**", it::messages::escape_display_text(command));
        let output = result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !output.trim().is_empty() {
            rendered.push_str("\n\n");
            rendered.push_str(&it::messages::escape_display_text(&output));
        }
        if let Some(details) = &result.details {
            if let Some(detail) =
                it::messages::render_tool_details_for_display("bash", details, *is_error)
            {
                rendered.push_str("\n\n");
                rendered.push_str(&detail);
            }
        }
        if let Some(elapsed) = tool.elapsed {
            rendered.push_str("\n\n");
            rendered.push_str(&format!("Took {:.1}s", elapsed.as_secs_f64()));
        }
        if *is_error {
            rendered.push_str("\n\n(error)");
        }
    } else {
        rendered.push_str("\n\n⏳ Running... (Esc to cancel)");
    }
    let _ = options;
    rendered
}

fn render_live_tool_call(
    tool_name: &str,
    args: &Value,
    options: it::messages::TranscriptRenderOptions,
) -> String {
    let mut assistant = AssistantMessage::new();
    assistant.set_content(vec![ContentBlock::tool_call("", tool_name, args.clone())]);
    it::messages::render_message_with_options(
        &pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)),
        options,
    )
    .map(|(_, rendered)| rendered)
    .unwrap_or_else(|| format!("⏳ **{tool_name}**"))
}

fn render_live_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    result: &pi_agent::tools::AgentToolResult,
    is_error: bool,
    options: it::messages::TranscriptRenderOptions,
) -> String {
    let message = ToolResultMessage::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: result.content.clone(),
        details: result.details.clone(),
        usage: result.usage.clone(),
        added_tool_names: (!result.added_tool_names.is_empty())
            .then(|| result.added_tool_names.clone()),
        is_error,
        timestamp: pi_ai::types::now_ms(),
    };
    it::messages::render_message_with_options(
        &pi_agent::types::AgentMessage::Core(Message::ToolResult(message)),
        options,
    )
    .map(|(_, rendered)| rendered)
    .unwrap_or_else(|| {
        let status = if is_error { "✗" } else { "✓" };
        format!("{status} **{tool_name}**")
    })
}

fn render_live_partial_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    partial: &Value,
    options: it::messages::TranscriptRenderOptions,
) -> Option<String> {
    let content = partial
        .get("content")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<ContentBlock>>(value).ok())?;
    if content.is_empty() {
        return None;
    }
    let message = ToolResultMessage::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        content,
        details: partial.get("details").cloned(),
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: pi_ai::types::now_ms(),
    };
    let (_, rendered) = it::messages::render_message_with_options(
        &pi_agent::types::AgentMessage::Core(Message::ToolResult(message)),
        options,
    )?;
    let completed_prefix = format!("✓ **{tool_name}**");
    let pending_prefix = format!("⏳ **{tool_name}**");
    Some(rendered.replacen(&completed_prefix, &pending_prefix, 1))
}

fn render_live_bash_execution(
    command: &str,
    output: &str,
    exclude_from_context: bool,
    options: it::messages::TranscriptRenderOptions,
) -> String {
    let message =
        pi_agent::types::AgentMessage::Custom(pi_agent::types::CustomAgentMessage::BashExecution {
            command: command.to_string(),
            output: output.to_string(),
            exit_code: None,
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: pi_ai::types::now_ms(),
            exclude_from_context: exclude_from_context.then_some(true),
        });
    it::messages::render_message_with_options(&message, options)
        .map(|(_, rendered)| rendered)
        .unwrap_or_else(|| format!("**$ {command}**\n\n⏳ Running... (Esc to cancel)"))
}

fn parse_bash_submission(text: &str) -> Option<(String, bool)> {
    let text = text.trim_start();
    let rest = text.strip_prefix('!')?;
    let exclude_from_context = rest.starts_with('!');
    let command = if exclude_from_context {
        &rest[1..]
    } else {
        rest
    };
    Some((command.trim_start().to_string(), exclude_from_context))
}

fn bash_execution_message(
    command: &str,
    capture: &pi_agent::tools::bash::BashCapture,
    exclude_from_context: bool,
) -> pi_agent::types::AgentMessage {
    pi_agent::types::AgentMessage::Custom(pi_agent::types::CustomAgentMessage::BashExecution {
        command: command.to_string(),
        output: capture.output.clone(),
        exit_code: capture.exit_code.map(i64::from),
        cancelled: capture.aborted,
        truncated: capture.truncated,
        full_output_path: capture.full_output_path.clone(),
        timestamp: pi_ai::types::now_ms(),
        exclude_from_context: exclude_from_context.then_some(true),
    })
}

/// Interactive session runtime (reuses the run/RPC wiring).
struct InteractiveRuntime {
    cwd: String,
    /// Stable builtin/provider base retained across explicit catalog refreshes.
    /// Each refresh recomposes a new facade from this registry so providers
    /// deleted from models.json cannot survive through a cloned provider map.
    model_registry: crate::core::model_registry::ModelRegistry,
    models: pi_ai::models::Models,
    /// Shared faux core for deterministic mode tests and the local provider;
    /// registering it through Models keeps deferred hooks available to the
    /// interactive runtime instead of bypassing the provider facade.
    faux_core: Option<pi_ai::providers::FauxProviderCore>,
    provider: String,
    model: Model,
    /// Canonical models enabled for Ctrl+P cycling by `/scoped-models`.
    /// Empty means the full available model catalog, matching Pi's default.
    scoped_models: Vec<String>,
    messages: Vec<pi_agent::types::AgentMessage>,
    session: JsonlSession<pi_agent::fs::StdFileSystem>,
    repo: JsonlSessionRepo<pi_agent::fs::StdFileSystem>,
    session_root: String,
    session_id: String,
    session_name: Option<String>,
    /// `--no-session` keeps the same session API in memory while preventing
    /// all durable session files and selectors from mutating disk.
    session_persistence: bool,
    system_prompt: Option<String>,
    tools_enabled: bool,
    builtin_tools_enabled: bool,
    default_tool_names: Option<Vec<String>>,
    extensions: LoadedExtensions,
    extension_resources: ResourceDiscovery,
    /// Skills loaded for this interactive session. The same list drives
    /// `/skill:name` expansion and the editor's skill-command completion.
    skills: Vec<crate::core::skills::Skill>,
    prompt_templates: Vec<crate::core::prompt_templates::PromptTemplate>,
    native_provider_ids: Vec<String>,
    extension_args: Args,
    extension_agent_dir: String,
    auto_resize_images: bool,
    block_images: bool,
    shell_command_prefix: Option<String>,
    shell_path: Option<String>,
    /// Effective provider transport settings captured when the retained
    /// interactive harness is built. Settings changes invalidate that
    /// harness so the next turn receives the new request options.
    transport: String,
    http_idle_timeout_ms: u64,
    provider_timeout_ms: Option<u64>,
    provider_max_retries: Option<u32>,
    max_retry_delay_ms: u64,
    websocket_connect_timeout_ms: Option<u64>,
    retry_policy: pi_ai::utils::RetryPolicy,
    /// Compaction settings captured when the retained harness is built. The
    /// harness installs its overflow-recovery hook at construction, so a
    /// settings change invalidates the idle harness and applies this value to
    /// the next turn without leaving the old hook enabled.
    compaction_settings: pi_agent::harness::compaction::CompactionSettings,
    /// Number of in-memory messages already persisted into the current
    /// session. Session-switch operations (resume/fork/clone) advance it so
    /// the exit persist only appends messages added after the switch.
    persisted_until: usize,
    /// Extension-requested active tool names, applied when the next actual
    /// interactive turn builds its tool list.
    active_tool_names: Option<Vec<String>>,
    /// Serialized session entries used to derive cache notices and cumulative
    /// footer/session usage before the deferred exit persist runs.
    cache_entries: Vec<Value>,
    /// Stateful harness for the active interactive session. Pi keeps one
    /// AgentSession alive across turns; retaining this harness is what keeps
    /// the agent queue, compaction state, hooks, and in-memory transcript
    /// continuous instead of reconstructing them from a lossy message list on
    /// every prompt.
    interactive_harness: Option<Arc<AgentHarness<pi_agent::fs::MemoryFs>>>,
    /// One listener is installed on the retained harness. Its callback slot
    /// is swapped for the active turn so repeated prompts do not accumulate
    /// duplicate transcript updates while still rendering stream events live.
    interactive_event_handler: Option<InteractiveEventHandler>,
    /// The companion listener slot for real tool execution lifecycle events.
    /// Keeping this separate preserves the existing assistant-event API for
    /// callers while allowing the interactive TUI to render tool blocks as
    /// they start, update, and settle.
    interactive_tool_event_handler: Option<InteractiveToolEventHandler>,
    /// Signal shutdown disposes extensions before terminal restoration; this
    /// flag prevents the final drop from emitting the lifecycle event twice.
    extensions_shutdown: bool,
}

fn interactive_compaction_settings(
    settings: &SettingsManager,
) -> pi_agent::harness::compaction::CompactionSettings {
    let (enabled, reserve_tokens, keep_recent_tokens) = settings.get_compaction_settings();
    pi_agent::harness::compaction::CompactionSettings {
        enabled,
        reserve_tokens,
        keep_recent_tokens,
    }
}

fn hide_thinking_for_level(settings: &SettingsManager, level: &str) -> bool {
    level == "off" || settings.get_hide_thinking_block()
}

fn invalidate_interactive_harness(runtime: &mut InteractiveRuntime) {
    runtime.interactive_harness = None;
    runtime.interactive_event_handler = None;
    runtime.interactive_tool_event_handler = None;
}

fn refresh_interactive_retry_settings(
    runtime: &mut InteractiveRuntime,
    settings: &SettingsManager,
) {
    let (provider_timeout_ms, provider_max_retries, max_retry_delay_ms) =
        settings.get_provider_retry_settings();
    runtime.retry_policy = crate::run::retry_policy_from_settings(settings);
    runtime.provider_timeout_ms = provider_timeout_ms;
    runtime.provider_max_retries =
        provider_max_retries.map(|retries| u32::try_from(retries).unwrap_or(u32::MAX));
    runtime.max_retry_delay_ms = max_retry_delay_ms;
    runtime.transport = settings.get_transport().to_string();
    runtime.http_idle_timeout_ms = settings.get_http_idle_timeout_ms().unwrap_or(300_000);
    runtime.websocket_connect_timeout_ms =
        settings.get_websocket_connect_timeout_ms().ok().flatten();
    runtime.shell_command_prefix = settings.get_shell_command_prefix().map(str::to_string);
    runtime.shell_path = settings.get_shell_path();
    invalidate_interactive_harness(runtime);
}

/// Reload models.json into the active interactive process. Pi performs this
/// through ModelRuntime refresh boundaries (not an automatic background file
/// watcher). Rebuilding from ModelRegistry's stable base prevents deleted or
/// malformed overlay state from leaking into the replacement facade.
fn reload_interactive_models(runtime: &mut InteractiveRuntime) -> Vec<String> {
    let config = crate::core::model_config::ModelConfig::load(
        crate::core::model_config::models_json_path().as_deref(),
    );
    apply_interactive_model_config(runtime, config)
}

fn apply_interactive_model_config(
    runtime: &mut InteractiveRuntime,
    config: crate::core::model_config::ModelConfig,
) -> Vec<String> {
    let config_error = config.get_error().map(str::to_owned);
    let registry = runtime.model_registry.with_config(config);
    let models = registry.into_models();

    let mut notes = Vec::new();
    if let Err(error) = register_loaded_native_providers(&models, &runtime.extensions) {
        notes.push(format!("native provider reload failed: {error}"));
    }
    if let Some(refreshed) = models.get_model(&runtime.provider, &runtime.model.id) {
        runtime.model = refreshed.clone();
    }
    runtime.model_registry = registry;
    runtime.models = models;
    invalidate_interactive_harness(runtime);
    if let Some(error) = config_error {
        notes.push(format!("models.json error: {error}"));
    }
    notes
}

/// Apply a queue-mode setting to the retained rich agent immediately. Pi's
/// settings callbacks mutate the live session agent; rebuilding the harness
/// here would discard its in-memory transcript and any queued messages.
fn apply_interactive_queue_mode(
    agent: &pi_agent::rich_agent::Agent,
    setting_id: &str,
    value: &str,
) {
    let mode = if value == "all" {
        pi_agent::rich_agent::QueueMode::All
    } else {
        pi_agent::rich_agent::QueueMode::OneAtATime
    };
    match setting_id {
        "steering-mode" => agent.set_steering_mode(mode),
        "follow-up-mode" => agent.set_follow_up_mode(mode),
        _ => {}
    }
}

/// A real llama.cpp operation kept outside the synchronous selector handler.
/// The model manager can continue rendering while the router loads/downloads
/// a model, and Ctrl-C/Escape can cancel the same request signal that the HTTP
/// client observes.
struct InteractiveLlamaOperation {
    signal: Arc<AtomicBool>,
    progress: Arc<Mutex<String>>,
    label: String,
    task: tokio::task::JoinHandle<Result<InteractiveLlamaOperationResult, String>>,
}

/// A direct `!`/`!!` command owns the same live loop as Pi's
/// `BashExecutionComponent`: output is updated incrementally, Ctrl+C/Escape
/// flips the real child-process abort flag, and the final structured capture
/// becomes a persisted `BashExecution` message.
struct InteractiveBashOperation {
    signal: Arc<AtomicBool>,
    command: String,
    exclude_from_context: bool,
    output: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<Result<pi_agent::tools::bash::BashCapture, String>>,
}

fn start_interactive_bash_operation(
    command: String,
    cwd: String,
    exclude_from_context: bool,
    stream_buffer: Arc<Mutex<String>>,
    command_prefix: Option<String>,
    shell_path: Option<String>,
) -> InteractiveBashOperation {
    let signal = Arc::new(AtomicBool::new(false));
    let output = Arc::new(Mutex::new(String::new()));
    let output_for_callback = Arc::clone(&output);
    let callback: pi_agent::tools::bash::BashOutputCallback = Arc::new(move |snapshot| {
        *output_for_callback
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = snapshot;
    });
    let signal_for_task = Arc::clone(&signal);
    let command_for_task = match command_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}\n{command}"),
        _ => command.clone(),
    };
    let task = tokio::spawn(async move {
        pi_agent::tools::bash::run_bash_with_output_and_shell(
            &command_for_task,
            &cwd,
            None,
            Some(signal_for_task),
            Some(callback),
            shell_path.as_deref(),
        )
        .await
        .map_err(|error| error.to_string())
    });
    *stream_buffer
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = render_live_bash_execution(
        &command,
        "",
        exclude_from_context,
        it::messages::TranscriptRenderOptions::default(),
    );
    InteractiveBashOperation {
        signal,
        command,
        exclude_from_context,
        output,
        task,
    }
}

#[derive(Debug)]
enum InteractiveLlamaOperationResult {
    Complete,
    Search(Vec<crate::core::llama::HuggingFaceModel>),
    Details(crate::core::llama::HuggingFaceModelDetails),
}

fn start_llama_operation(
    label: String,
    task: tokio::task::JoinHandle<Result<InteractiveLlamaOperationResult, String>>,
    signal: Arc<AtomicBool>,
    progress: Arc<Mutex<String>>,
) -> InteractiveLlamaOperation {
    InteractiveLlamaOperation {
        signal,
        progress,
        label,
        task,
    }
}

fn start_llama_model_operation(
    client: crate::core::llama::LlamaClient,
    action: crate::core::llama::LlamaManagerAction,
) -> InteractiveLlamaOperation {
    let signal = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(String::new()));
    let task_signal = signal.clone();
    let task_progress = progress.clone();
    let label = match &action {
        crate::core::llama::LlamaManagerAction::Model { id, .. } => id.clone(),
        _ => "llama.cpp operation".to_owned(),
    };
    let task = tokio::spawn(async move {
        crate::interactive::llama::execute_model_action(
            &client,
            &action,
            task_signal,
            task_progress,
        )
        .await
        .map(|_| InteractiveLlamaOperationResult::Complete)
    });
    start_llama_operation(label, task, signal, progress)
}

fn start_llama_download_operation(
    client: crate::core::llama::LlamaClient,
    spec: String,
) -> InteractiveLlamaOperation {
    let signal = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(String::new()));
    let task_signal = signal.clone();
    let task_progress = progress.clone();
    let label = spec.clone();
    let task = tokio::spawn(async move {
        crate::interactive::llama::download_huggingface_model(
            &client,
            &spec,
            task_signal,
            task_progress,
        )
        .await
        .map(|_| InteractiveLlamaOperationResult::Complete)
    });
    start_llama_operation(label, task, signal, progress)
}

fn start_llama_load_operation(
    client: crate::core::llama::LlamaClient,
    target: String,
    loaded: Vec<String>,
    replace: bool,
) -> InteractiveLlamaOperation {
    let signal = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(String::new()));
    let task_signal = signal.clone();
    let task_progress = progress.clone();
    let label = target.clone();
    let task = tokio::spawn(async move {
        crate::interactive::llama::execute_load_with_restore(
            &client,
            &target,
            &loaded,
            replace,
            task_signal,
            task_progress,
        )
        .await
        .map(|_| InteractiveLlamaOperationResult::Complete)
    });
    start_llama_operation(label, task, signal, progress)
}

fn start_huggingface_search_operation(query: String) -> Result<InteractiveLlamaOperation, String> {
    let client = crate::core::llama::HuggingFaceClient::new(
        crate::core::llama::find_huggingface_token().as_deref(),
    )
    .map_err(|error| format!("Hugging Face setup failed: {error}"))?;
    let signal = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(String::new()));
    let task_signal = signal.clone();
    let task_query = query.clone();
    let task = tokio::spawn(async move {
        client
            .search(&task_query, Some(task_signal))
            .await
            .map(InteractiveLlamaOperationResult::Search)
            .map_err(|error| format!("Hugging Face search failed: {error}"))
    });
    Ok(start_llama_operation(
        format!("Hugging Face search {query:?}"),
        task,
        signal,
        progress,
    ))
}

fn start_huggingface_details_operation(
    model_id: String,
) -> Result<InteractiveLlamaOperation, String> {
    let client = crate::core::llama::HuggingFaceClient::new(
        crate::core::llama::find_huggingface_token().as_deref(),
    )
    .map_err(|error| format!("Hugging Face setup failed: {error}"))?;
    let signal = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(String::new()));
    let task_signal = signal.clone();
    let task_model_id = model_id.clone();
    let task = tokio::spawn(async move {
        client
            .details(&task_model_id, Some(task_signal))
            .await
            .map(InteractiveLlamaOperationResult::Details)
            .map_err(|error| format!("Hugging Face model details failed: {error}"))
    });
    Ok(start_llama_operation(
        format!("Hugging Face details {model_id}"),
        task,
        signal,
        progress,
    ))
}

impl Drop for InteractiveRuntime {
    fn drop(&mut self) {
        self.shutdown_extensions("quit");
    }
}

impl InteractiveRuntime {
    fn shutdown_extensions(&mut self, reason: &str) {
        if self.extensions_shutdown {
            return;
        }
        let _ = self.extensions.runner.emit_session_shutdown(reason);
        self.extensions
            .runner
            .invalidate(Some("interactive mode shutdown"));
        self.extensions_shutdown = true;
    }
}

/// Own raw/alternate-screen cleanup for every exit after the TUI activates.
/// The explicit cleanup at the normal loop boundary remains useful for prompt
/// handoff, while this guard covers startup failures, input errors, and early
/// returns without leaving the parent shell in raw mode.
struct InteractiveTerminalGuard {
    terminal: Arc<Mutex<TerminalBackend>>,
}

impl Drop for InteractiveTerminalGuard {
    fn drop(&mut self) {
        let mut terminal = match self.terminal.lock() {
            Ok(terminal) => terminal,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = terminal.leave_raw();
    }
}

/// Own one terminal reader for the entire interactive session.
///
/// Reading stdin through a new `spawn_blocking` task for every streaming turn
/// loses input at the turn boundary: the abandoned blocking task can consume
/// the first key of the next prompt while its result is no longer observed.
/// Pi keeps one input loop alive for the whole TUI, so the Rust port does the
/// same and shares its lossless event queue between the idle loop, streaming
/// turns, and modal/auth flows.
struct InteractiveInputReader {
    terminal: Arc<Mutex<TerminalBackend>>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<Result<pi_tui::terminal::TerminalEvent, String>>,
    pending_events: VecDeque<Result<pi_tui::terminal::TerminalEvent, String>>,
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Preserve a key that arrived in the same scheduling window as bash
/// completion. The outer loop finalizes the operation before reading this
/// queued event again, so a just-submitted command cannot be lost to the
/// `tokio::select!` input/completion race.
fn defer_input_until_bash_completion(
    pending_events: &mut VecDeque<Result<pi_tui::terminal::TerminalEvent, String>>,
    key: String,
) {
    pending_events.push_front(Ok(pi_tui::terminal::TerminalEvent::Key(key)));
}

impl InteractiveInputReader {
    fn start(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = Arc::clone(&stop);
        let task_terminal = Arc::clone(&terminal);
        let task = tokio::task::spawn_blocking(move || {
            while !task_stop.load(Ordering::Acquire) {
                #[cfg(unix)]
                let event = {
                    // Wait for fd readiness without holding the terminal
                    // mutex. The renderer can therefore write a frame while
                    // the reader is idle, and a real key wakes this worker
                    // immediately instead of waiting behind a 1 ms sleep.
                    let (fd, timeout) = match task_terminal.lock() {
                        Ok(terminal) => (terminal.stdin_fd(), terminal.input_wait_timeout_hint()),
                        Err(_) => {
                            let _ = sender.send(Err("terminal lock poisoned".to_string()));
                            break;
                        }
                    };
                    match TerminalBackend::poll_input_fd(fd, Some(timeout)) {
                        Ok(_) => match task_terminal.lock() {
                            Ok(mut terminal) => terminal
                                .try_next_event()
                                .map_err(|error| format!("read terminal input: {error}")),
                            Err(_) => Err("terminal lock poisoned".to_string()),
                        },
                        Err(error) => Err(format!("poll terminal input: {error}")),
                    }
                };

                #[cfg(not(unix))]
                let event = match task_terminal.lock() {
                    Ok(mut terminal) => terminal
                        .try_next_event()
                        .map_err(|error| format!("read terminal input: {error}")),
                    Err(_) => Err("terminal lock poisoned".to_string()),
                };

                match event {
                    Ok(Some(event)) => {
                        let reached_eof = matches!(
                            &event,
                            pi_tui::terminal::TerminalEvent::Key(key) if key.is_empty()
                        ) && task_terminal
                            .lock()
                            .map(|terminal| terminal.stdin_eof())
                            .unwrap_or(true);
                        if reached_eof {
                            let _ = sender.send(Ok(event));
                            break;
                        }
                        // Poll timeouts are an internal wake-up, not user
                        // input. The Unix reader already waits outside the
                        // terminal mutex, so immediately checking again is
                        // safe and keeps resize/escape deadlines precise.
                        if matches!(
                            &event,
                            pi_tui::terminal::TerminalEvent::Key(key) if key.is_empty()
                        ) {
                            continue;
                        }
                        if sender.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        // Readiness polling above already yielded without
                        // holding the terminal mutex. No fixed sleep belongs
                        // on the input path: the next fd event or bounded
                        // resize/deadline wake-up drives the next iteration.
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });

        Self {
            terminal,
            receiver,
            pending_events: VecDeque::new(),
            stop,
            task: Some(task),
        }
    }

    async fn recv(&mut self) -> Option<Result<pi_tui::terminal::TerminalEvent, String>> {
        let event = self.recv_raw().await?;
        match event {
            // Some tmux versions write legacy M-Enter as two PTY writes. The
            // terminal parser quite correctly flushes the lone ESC after its
            // short timeout, but Pi's action matcher still sees the logical
            // Alt+Enter pair. Reassemble only this ambiguous two-byte pair at
            // the mode boundary; ordinary ESC remains an immediate cancel
            // after the small compatibility window.
            Ok(pi_tui::terminal::TerminalEvent::Key(raw)) if raw == "\x1b" => {
                match tokio::time::timeout(std::time::Duration::from_millis(25), self.recv_raw())
                    .await
                {
                    Ok(Some(Ok(pi_tui::terminal::TerminalEvent::Key(next))))
                        if next == "\r" || next == "\n" =>
                    {
                        Some(Ok(pi_tui::terminal::TerminalEvent::Key(format!(
                            "\x1b{next}"
                        ))))
                    }
                    Ok(Some(next)) => {
                        self.pending_events.push_front(next);
                        Some(Ok(pi_tui::terminal::TerminalEvent::Key(raw)))
                    }
                    Ok(None) | Err(_) => Some(Ok(pi_tui::terminal::TerminalEvent::Key(raw))),
                }
            }
            other => Some(other),
        }
    }

    async fn recv_raw(&mut self) -> Option<Result<pi_tui::terminal::TerminalEvent, String>> {
        if let Some(event) = self.pending_events.pop_front() {
            Some(event)
        } else {
            self.receiver.recv().await
        }
    }

    fn try_recv_raw(&mut self) -> Option<Result<pi_tui::terminal::TerminalEvent, String>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Some(event);
        }
        match self.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                Some(Err("terminal input reader stopped".to_string()))
            }
        }
    }

    /// Take one scheduling window of plain editor text without consuming the
    /// next control/resize event. A non-text event is put back at the front so
    /// the normal dispatcher retains exact ordering and its ESC reassembly.
    fn take_immediate_editor_keys(&mut self) -> Vec<String> {
        let mut keys = Vec::new();
        while keys.len() < MAX_IMMEDIATE_EDITOR_EVENTS {
            let Some(event) = self.try_recv_raw() else {
                break;
            };
            match take_immediate_editor_key(event, &mut self.pending_events) {
                Some(key) => keys.push(key),
                None => break,
            }
        }
        keys
    }

    fn pending_cancel(&mut self) -> bool {
        let mut cancelled = false;
        while let Ok(event) = self.receiver.try_recv() {
            let is_cancel = matches!(
                &event,
                Ok(pi_tui::terminal::TerminalEvent::Key(raw))
                    if {
                        let key = parse_key(raw);
                        key.base == "esc"
                            || key.base == "escape"
                            || (key.ctrl && key.base == "c")
                    }
            );
            if is_cancel {
                cancelled = true;
            } else {
                // Login/logout temporarily take over the terminal reader.
                // Preserve every unrelated event so a key typed around that
                // hand-off is delivered to the editor afterward instead of
                // being silently consumed by cancellation probing.
                self.pending_events.push_back(event);
            }
        }
        cancelled
    }

    async fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    async fn restart(&mut self) {
        self.stop_worker().await;
        let pending_events = std::mem::take(&mut self.pending_events);
        let replacement = Self::start(self.terminal.clone());
        *self = replacement;
        self.pending_events = pending_events;
    }

    async fn shutdown(mut self) {
        self.stop_worker().await;
    }
}

#[cfg(unix)]
mod unix_suspend {
    use std::io;
    use std::os::raw::c_int;

    // These are the POSIX signal numbers used by the Unix targets supported
    // by this binary. Keeping the values here avoids adding a new dependency
    // just for process-control syscalls in this narrowly scoped path.
    #[cfg(all(
        any(target_os = "linux", target_os = "android", target_os = "emscripten"),
        any(target_arch = "mips", target_arch = "mips64")
    ))]
    pub const SIGCONT: c_int = 25;
    #[cfg(all(
        any(target_os = "linux", target_os = "android", target_os = "emscripten"),
        any(target_arch = "mips", target_arch = "mips64")
    ))]
    const SIGTSTP: c_int = 24;
    #[cfg(all(
        any(target_os = "linux", target_os = "android", target_os = "emscripten"),
        any(target_arch = "sparc", target_arch = "sparc64")
    ))]
    pub const SIGCONT: c_int = 19;
    #[cfg(all(
        any(target_os = "linux", target_os = "android", target_os = "emscripten"),
        any(target_arch = "sparc", target_arch = "sparc64")
    ))]
    const SIGTSTP: c_int = 18;
    #[cfg(all(
        any(target_os = "linux", target_os = "android", target_os = "emscripten"),
        not(any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "sparc",
            target_arch = "sparc64"
        ))
    ))]
    pub const SIGCONT: c_int = 18;
    #[cfg(all(
        any(target_os = "linux", target_os = "android", target_os = "emscripten"),
        not(any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "sparc",
            target_arch = "sparc64"
        ))
    ))]
    const SIGTSTP: c_int = 20;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "aix",
        target_os = "cygwin",
        target_os = "hurd"
    ))]
    pub const SIGCONT: c_int = 19;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "aix",
        target_os = "cygwin",
        target_os = "hurd"
    ))]
    const SIGTSTP: c_int = 18;
    #[cfg(any(target_os = "solaris", target_os = "illumos", target_os = "nto"))]
    pub const SIGCONT: c_int = 25;
    #[cfg(any(target_os = "solaris", target_os = "illumos", target_os = "nto"))]
    const SIGTSTP: c_int = 24;
    #[cfg(target_os = "haiku")]
    pub const SIGCONT: c_int = 12;
    #[cfg(target_os = "haiku")]
    const SIGTSTP: c_int = 13;

    const SIGINT: c_int = 2;
    const SIG_IGN: usize = 1;
    const SIG_ERR: usize = usize::MAX;

    unsafe extern "C" {
        fn kill(pid: c_int, signal: c_int) -> c_int;
        fn signal(signal: c_int, handler: usize) -> usize;
    }

    pub struct IgnoredSigint {
        previous_handler: usize,
    }

    impl IgnoredSigint {
        pub fn install() -> io::Result<Self> {
            // SAFETY: SIGINT and SIG_IGN are valid POSIX signal/disposition
            // values, and this call is made outside a signal handler.
            let previous_handler = unsafe { signal(SIGINT, SIG_IGN) };
            if previous_handler == SIG_ERR {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { previous_handler })
            }
        }
    }

    impl Drop for IgnoredSigint {
        fn drop(&mut self) {
            // SAFETY: the saved value came from signal(2), so restoring it is
            // valid for the lifetime of this guard.
            let _ = unsafe { signal(SIGINT, self.previous_handler) };
        }
    }

    pub fn suspend_process_group() -> io::Result<()> {
        // pid 0 targets the caller's process group, matching the upstream
        // process.kill(0, "SIGTSTP") behavior and suspending active children.
        // SAFETY: kill has no Rust-owned pointers and the signal is valid.
        if unsafe { kill(0, SIGTSTP) } == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(unix)]
async fn suspend_interactive(
    terminal: &Arc<Mutex<TerminalBackend>>,
    input: &mut InteractiveInputReader,
    use_alt_screen: bool,
    sigcont: &mut tokio::signal::unix::Signal,
) -> Result<(), String> {
    // The reader may be inside poll(2), holding the terminal mutex between
    // reads. Stop it before changing terminal modes so no input can race the
    // handoff and no stale reader remains after SIGCONT.
    input.stop_worker().await;
    let _ignore_sigint = match unix_suspend::IgnoredSigint::install() {
        Ok(guard) => guard,
        Err(error) => {
            input.restart().await;
            return Err(format!("ignore SIGINT while suspended: {error}"));
        }
    };

    let leave_raw_result = {
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .leave_raw()
    };
    if let Err(error) = leave_raw_result {
        input.restart().await;
        return Err(format!("restore terminal before suspend: {error}"));
    }
    // Give terminal multiplexers a scheduling turn to observe the cooked
    // termios state before the process group is stopped.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Keep a ref'ed timer future in the event loop while stopped. This mirrors
    // the upstream keep-alive and ensures the resumed process continues into
    // the SIGCONT wait instead of reaching an otherwise idle runtime exit.
    let mut suspend_keep_alive = tokio::time::interval(std::time::Duration::from_secs(1 << 30));
    if let Err(error) = unix_suspend::suspend_process_group() {
        let restore = terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enter_raw_with_alt_screen(use_alt_screen);
        input.restart().await;
        return Err(match restore {
            Ok(()) => format!("suspend process group: {error}"),
            Err(restore_error) => {
                format!("suspend process group: {error}; restore terminal: {restore_error}")
            }
        });
    }

    loop {
        tokio::select! {
            signal = sigcont.recv() => {
                if signal.is_some() {
                    break;
                }
                let restore = terminal
                    .lock().unwrap_or_else(|error| error.into_inner())
                    .enter_raw_with_alt_screen(use_alt_screen);
                input.restart().await;
                return Err(match restore {
                    Ok(()) => "SIGCONT watcher stopped while suspended".to_string(),
                    Err(error) => format!("SIGCONT watcher stopped; restore terminal: {error}"),
                });
            }
            _ = suspend_keep_alive.tick() => {}
        }
    }

    let restore = terminal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .enter_raw_with_alt_screen(use_alt_screen);
    input.restart().await;
    restore.map_err(|error| format!("restore terminal after SIGCONT: {error}"))
}

fn quote_resume_argument(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-./~:@".contains(character))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Build the same resumable-session hint emitted by Pi after a clean
/// fullscreen shutdown. The metadata/file checks intentionally suppress a
/// misleading hint for ephemeral or not-yet-persisted sessions.
async fn format_resume_command(runtime: &InteractiveRuntime) -> Option<String> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) || !runtime.session_persistence {
        return None;
    }

    let session_path = runtime.session.get_metadata().await.path;
    if session_path.is_empty() || !std::path::Path::new(&session_path).is_file() {
        return None;
    }

    let default_session_root = crate::config::get_session_dir()
        .to_string_lossy()
        .into_owned();
    let mut command = crate::config::APP_NAME.to_string();
    if runtime.session_root != default_session_root {
        command.push_str(" --session-dir ");
        command.push_str(&quote_resume_argument(&runtime.session_root));
    }
    command.push_str(" --session ");
    command.push_str(&quote_resume_argument(&runtime.session_id));
    Some(command)
}

fn should_exit_on_key(key: &TuiKey, editor_text: &str) -> bool {
    key.ctrl && key.base == "d" && editor_text.is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoubleEscapeAction {
    Tree,
    Fork,
}

/// Match Pi's empty-editor double-Escape window. The first Escape is consumed
/// and arms a 500 ms timer; only the second Escape inside that window routes
/// to the configured selector.
fn resolve_double_escape(
    configured_action: &str,
    last_escape: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<DoubleEscapeAction> {
    if configured_action == "none" {
        *last_escape = None;
        return None;
    }

    if last_escape.is_some_and(|previous| {
        now.saturating_duration_since(previous) < std::time::Duration::from_millis(500)
    }) {
        *last_escape = None;
        return match configured_action {
            "fork" => Some(DoubleEscapeAction::Fork),
            "tree" => Some(DoubleEscapeAction::Tree),
            _ => None,
        };
    }

    *last_escape = Some(now);
    None
}

fn resumable_sessions(
    sessions: Vec<pi_agent::session::types::SessionMetadata>,
    current_id: &str,
    current_path: Option<&str>,
) -> Vec<pi_agent::session::types::SessionMetadata> {
    let current_path = current_path
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    sessions
        .into_iter()
        .filter(|session| {
            if let Some(current_path) = current_path.as_ref() {
                return !same_session_path(Path::new(&session.path), current_path);
            }
            session.id != current_id
        })
        .collect()
}

fn same_session_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Build the tools for one interactive turn and refresh the extension host
/// catalog from the exact set that is available to that turn.
fn interactive_turn_tools(runtime: &InteractiveRuntime) -> Vec<pi_agent::tools::AgentTool> {
    let mut tools = if runtime.tools_enabled && runtime.builtin_tools_enabled {
        vec![
            pi_agent::tools::bash_tool_with_options(
                runtime.cwd.clone(),
                runtime.shell_command_prefix.clone(),
                runtime.shell_path.clone(),
            ),
            pi_agent::tools::read_tool_with_options(
                runtime.cwd.clone(),
                pi_agent::tools::image::ProcessImageOptions {
                    auto_resize_images: runtime.auto_resize_images,
                    ..Default::default()
                },
            ),
            pi_agent::tools::write_tool(runtime.cwd.clone()),
            pi_agent::tools::edit_tool(runtime.cwd.clone()),
            crate::core::tools::ls_tool(runtime.cwd.clone()),
            crate::core::tools::find_tool(runtime.cwd.clone()),
            crate::core::tools::grep_tool(runtime.cwd.clone()),
        ]
    } else {
        Vec::new()
    };
    install_tools(&runtime.extensions, &mut tools, runtime.tools_enabled);
    let mut active_names = if let Some(explicit) = runtime.extension_args.tools.clone() {
        explicit
    } else if runtime.extension_args.no_tools {
        Vec::new()
    } else if runtime.extension_args.no_builtin_tools {
        tools
            .iter()
            .filter(|tool| !is_builtin_interactive_tool(&tool.tool.name))
            .map(|tool| tool.tool.name.clone())
            .collect()
    } else {
        runtime.default_tool_names.clone().unwrap_or_else(|| {
            ["read", "bash", "edit", "write"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
    };
    if runtime.extension_args.tools.is_none()
        && !runtime.extension_args.no_tools
        && !runtime.extension_args.no_builtin_tools
    {
        active_names.extend(
            tools
                .iter()
                .filter(|tool| !is_builtin_interactive_tool(&tool.tool.name))
                .map(|tool| tool.tool.name.clone()),
        );
    }
    if let Some(excluded) = &runtime.extension_args.exclude_tools {
        active_names.retain(|name| !excluded.iter().any(|excluded| excluded == name));
    }
    active_names.sort();
    active_names.dedup();
    tools.retain(|tool| active_names.iter().any(|name| name == &tool.tool.name));
    if let Some(active_tool_names) = &runtime.active_tool_names {
        tools.retain(|tool| active_tool_names.iter().any(|name| name == &tool.tool.name));
    }
    tools
}

fn is_builtin_interactive_tool(name: &str) -> bool {
    matches!(
        name,
        "bash" | "read" | "write" | "edit" | "ls" | "find" | "grep"
    )
}

/// Build the interactive system prompt from the same skill/resource surface
/// as print mode. Extension-provided skills are temporary and are therefore
/// supplied directly rather than persisted into settings.
fn interactive_system_prompt(
    args: &Args,
    cwd: &str,
    agent_dir: &str,
    settings: &SettingsManager,
    resources: &ResourceDiscovery,
) -> Option<String> {
    Some(crate::run::assemble_run_system_prompt(
        args,
        cwd,
        std::path::Path::new(agent_dir),
        settings,
        resources,
    ))
}

/// Load the session-visible skills once for both prompt and interactive
/// command use. This mirrors `run::build_skills_block` while retaining the
/// actual records needed by autocomplete and `/skill:name` expansion.
fn load_interactive_skills(
    args: &Args,
    cwd: &str,
    agent_dir: &str,
    settings: &SettingsManager,
    resources: &ResourceDiscovery,
) -> Vec<crate::core::skills::Skill> {
    let mut skill_paths = if args.no_skills {
        Vec::new()
    } else {
        settings.get_skill_paths()
    };
    skill_paths.extend(args.skills.iter().cloned());
    skill_paths.extend(resources.resolved_skill_paths(cwd));
    let options = crate::core::skills::LoadSkillsOptions {
        cwd: cwd.to_string(),
        agent_dir: agent_dir.to_string(),
        skill_paths,
    };
    let (skills, diagnostics) = if args.no_skills {
        crate::core::skills::load_skills_without_defaults(options)
    } else {
        crate::core::skills::load_skills(options)
    };
    for diagnostic in diagnostics {
        tracing::warn!(
            path = ?diagnostic.path,
            message = %diagnostic.message,
            "skill load diagnostic"
        );
    }
    skills
}

/// Refresh the process-local theme registry from settings, CLI paths, and
/// extension-discovered resource paths. The registry is replaced on every
/// load/reload so removed extension themes cannot remain selectable.
fn register_interactive_themes(
    args: &Args,
    settings: &SettingsManager,
    resources: &ResourceDiscovery,
    cwd: &str,
) {
    let mut paths = if args.no_themes {
        Vec::new()
    } else {
        settings.get_theme_paths()
    };
    paths.extend(args.themes.iter().cloned());
    let mut sources = paths
        .into_iter()
        .map(|path| (path, None))
        .collect::<Vec<_>>();
    sources.extend(resources.theme_resources.iter().map(|resource| {
        (
            resource.resolved_path(cwd),
            Some(resource.source_info.clone()),
        )
    }));
    if resources.theme_resources.is_empty() {
        sources.extend(
            resources
                .resolved_theme_paths(cwd)
                .into_iter()
                .map(|path| (path, None)),
        );
    }
    let _ = crate::theme::register_theme_sources(&sources, std::path::Path::new(cwd));
}

fn load_interactive_theme(name: &str) {
    it::tui_theme::load_theme(name);
    it::tui_theme::watch_active_theme();
}

fn load_interactive_theme_checked(name: &str) -> Result<(), String> {
    it::tui_theme::try_load_theme(name)?;
    it::tui_theme::watch_active_theme();
    Ok(())
}

/// Resolve the persisted theme setting before loading it. A slash-separated
/// setting is the upstream automatic light/dark form; the TUI theme registry
/// itself only accepts the active concrete theme name.
fn resolve_interactive_theme_name(setting: &str) -> Result<String, String> {
    crate::theme::resolve_theme_setting(Some(setting), &crate::theme::default_theme())
        .ok_or_else(|| format!("invalid theme setting: {setting}"))
}

fn load_interactive_theme_setting_checked(setting: &str) -> Result<(), String> {
    let name = resolve_interactive_theme_name(setting)?;
    load_interactive_theme_checked(&name)
}

fn load_interactive_theme_setting(setting: &str) {
    let name = resolve_interactive_theme_name(setting)
        .unwrap_or_else(|_| crate::theme::DEFAULT_THEME.to_string());
    load_interactive_theme(&name);
}

fn refresh_interactive_theme_views(
    transcript_md: &Arc<Mutex<Markdown>>,
    transcript_view: &Arc<Mutex<InteractiveTranscriptView>>,
) {
    transcript_md
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .set_theme(it::tui_theme::markdown_theme());
    transcript_view
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .invalidate_theme();
}

fn loaded_native_provider_ids(loaded: &LoadedExtensions) -> Vec<String> {
    loaded
        .runtime
        .lock()
        .map(|runtime| {
            runtime
                .pending_native_provider_registrations
                .iter()
                .map(|registration| registration.provider.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Rebuild the mode-scoped extension runtime after `/reload`. The upstream
/// resource loader re-evaluates every extension with a fresh module cache;
/// replacing the runner here also tears down the persistent bridge process and
/// refreshes native providers and the synchronous host tool catalog.
fn reload_extensions(
    runtime: &mut InteractiveRuntime,
    settings: &SettingsManager,
    thinking_level: &str,
) -> Vec<String> {
    invalidate_interactive_harness(runtime);
    shutdown_extensions_before_session_replace(runtime, "reload", None);
    replace_extensions(runtime, settings, thinking_level, "reload", None, None)
}

/// Shut down the current extension runtime while it still belongs to the old
/// session. Replacement callers must invoke this before assigning
/// `runtime.session`; reload uses the same helper even though its session is
/// not replaced.
fn shutdown_extensions_before_session_replace(
    runtime: &InteractiveRuntime,
    reason: &str,
    target_session_file: Option<&str>,
) {
    let _ = runtime
        .extensions
        .runner
        .emit_session_shutdown_with_target(reason, target_session_file);
    runtime
        .extensions
        .runner
        .invalidate(Some("interactive extension replacement"));
}

/// Replace the mode-scoped extension runtime for `/reload` and session
/// replacement. The caller has already shut down and invalidated the old
/// runner so no old-session hook can observe replacement state.
fn replace_extensions(
    runtime: &mut InteractiveRuntime,
    settings: &SettingsManager,
    thinking_level: &str,
    reason: &str,
    previous_session_file: Option<&str>,
    _target_session_file: Option<&str>,
) -> Vec<String> {
    let previous_flag_values = runtime.extensions.runner.get_flag_values();
    let reloaded_extensions = load_for_mode_with_reason_and_flags_and_previous(
        &runtime.extension_args,
        settings,
        &runtime.cwd,
        &runtime.extension_agent_dir,
        "interactive",
        true,
        runtime.session_name.clone(),
        thinking_level.to_string(),
        reason,
        Some(previous_flag_values),
        previous_session_file,
    );
    let mut notes = reloaded_extensions
        .errors
        .iter()
        .map(|error| format!("{}: {}", error.path, error.error))
        .collect::<Vec<_>>();
    for provider_id in runtime.native_provider_ids.drain(..) {
        runtime.models.delete_provider(&provider_id);
    }
    let reloaded_extensions = reloaded_extensions;
    runtime.extension_resources = reloaded_extensions.resources.clone();
    let _old_extensions = std::mem::replace(&mut runtime.extensions, reloaded_extensions);
    runtime.skills = load_interactive_skills(
        &runtime.extension_args,
        &runtime.cwd,
        &runtime.extension_agent_dir,
        settings,
        &runtime.extension_resources,
    );
    register_interactive_themes(
        &runtime.extension_args,
        settings,
        &runtime.extension_resources,
        &runtime.cwd,
    );
    runtime.system_prompt = interactive_system_prompt(
        &runtime.extension_args,
        &runtime.cwd,
        &runtime.extension_agent_dir,
        settings,
        &runtime.extension_resources,
    );
    runtime.prompt_templates = crate::run::load_prompt_templates_for_run(
        &runtime.extension_args,
        &runtime.cwd,
        std::path::Path::new(&runtime.extension_agent_dir),
        &runtime.extension_resources,
    );
    runtime.native_provider_ids = loaded_native_provider_ids(&runtime.extensions);
    match register_loaded_native_providers(&runtime.models, &runtime.extensions) {
        Ok(count) if count > 0 => notes.push(format!("reloaded {count} native provider(s)")),
        Ok(_) => {}
        Err(error) => notes.push(format!("native provider reload failed: {error}")),
    }
    let _ = interactive_turn_tools(runtime);
    notes
}

/// Refresh the retained startup/resource presentation after the owner swaps
/// extensions or sessions. The component keeps its expansion state, while the
/// resource summary is rebuilt from the same runtime that feeds the system
/// prompt and autocomplete provider.
fn refresh_startup_presentation(
    startup_presentation: Option<&Arc<Mutex<it::startup::InteractiveStartupPresentation>>>,
    runtime: &InteractiveRuntime,
    args: &Args,
    settings: &SettingsManager,
) {
    let Some(startup) = startup_presentation else {
        return;
    };
    let mut startup = startup.lock().unwrap_or_else(|error| error.into_inner());
    let expanded = startup.is_expanded();
    startup.refresh(
        crate::config::VERSION,
        &runtime.cwd,
        &runtime.extension_agent_dir,
        args,
        settings,
        &runtime.extension_resources,
        runtime.extensions.runner.extensions(),
        &runtime.extensions.errors,
        &runtime.prompt_templates,
    );
    startup.set_expanded(expanded);
}

fn session_switch_allowed(
    runtime: &InteractiveRuntime,
    reason: &str,
    target_session_file: Option<&str>,
) -> bool {
    match runtime
        .extensions
        .runner
        .emit_session_before_switch(reason, target_session_file)
    {
        Ok(true) => false,
        Ok(false) => true,
        Err(errors) => {
            for error in errors {
                tracing::warn!(
                    extension = %error.extension_path,
                    event = %error.event,
                    error = %error.error,
                    "extension session-switch handler failed"
                );
            }
            true
        }
    }
}

fn session_fork_allowed(runtime: &InteractiveRuntime, entry_id: &str, position: &str) -> bool {
    match runtime
        .extensions
        .runner
        .emit_session_before_fork(entry_id, position)
    {
        Ok(true) => false,
        Ok(false) => true,
        Err(errors) => {
            for error in errors {
                tracing::warn!(
                    extension = %error.extension_path,
                    event = %error.event,
                    error = %error.error,
                    "extension session-before-fork handler failed"
                );
            }
            true
        }
    }
}

/// Execute an extension command that is not one of the built-in slash
/// commands. Built-ins deliberately win name conflicts so existing interactive
/// behavior is unchanged.
fn execute_interactive_extension_command(
    runtime: &InteractiveRuntime,
    submitted: &str,
) -> Option<String> {
    let (Some(name), args) = it::slash::parse_invocation(submitted.trim()) else {
        return None;
    };
    if it::slash::find_command(name).is_some() {
        return None;
    }
    let mut runner = runtime.extensions.runner.as_ref().clone();
    runner.get_command(name)?;
    let result = runner.execute_command(name, args);
    Some(match result {
        Ok(Some(value)) => {
            let rendered = value.to_string();
            let rendered = rendered.chars().take(240).collect::<String>();
            format!("/{name}: {rendered}")
        }
        Ok(None) => format!("/{name} completed"),
        Err(error) => format!("/{name} failed: {error}"),
    })
}

/// Build the upstream `/fork` user-message selector from durable session
/// entries. Forking before a user message is the only valid `before` target;
/// `/clone` remains the separate current-leaf `at` operation.
async fn fork_selector_items(
    session: &JsonlSession<pi_agent::fs::StdFileSystem>,
) -> Vec<SelectItem> {
    let entries = session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|entry| {
            let pi_agent::session::types::Entry::Message { id, message, .. } = entry else {
                return None;
            };
            let pi_agent::types::AgentMessage::Core(Message::User(user)) = message else {
                return None;
            };
            let text = pi_agent::agent::user_content_text(&user);
            if text.trim().is_empty() {
                return None;
            }
            let label = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let label = label.chars().take(80).collect::<String>();
            Some(SelectItem::new(id, label, Some(text)))
        })
        .collect()
}

/// Read the durable session tree and construct the same selector used by the
/// `/tree` command and the empty-editor double-Escape shortcut.
async fn tree_selector_for_session(
    session: &JsonlSession<pi_agent::fs::StdFileSystem>,
    terminal_height: usize,
    filter_mode: it::tree_selector::TreeFilterMode,
) -> Result<Option<it::tree_selector::TreeSelector>, &'static str> {
    let entries = session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|_| "tree: failed to read session entries")?;
    if entries.is_empty() {
        return Ok(None);
    }

    let current_leaf = session.get_leaf_id().await.ok().flatten();
    let mut labels = std::collections::HashMap::new();
    for entry in &entries {
        if let Some(label) = session.get_label(entry.id()).await {
            labels.insert(entry.id().to_string(), label);
        }
    }
    Ok(Some(it::tree_selector::TreeSelector::new_with_filter_mode(
        entries,
        labels,
        current_leaf,
        terminal_height,
        filter_mode,
    )))
}

/// Execute a selected fork/clone after the cancellable hook has allowed it.
struct InteractiveForkResult {
    status: String,
    /// `/fork` restores the selected user prompt into the editor; `/clone`
    /// clears it after creating the new session.
    editor_text: Option<String>,
}

struct InteractiveForkContext<'a> {
    settings: &'a SettingsManager,
    thinking_level: &'a str,
    transcript_md: &'a Arc<Mutex<Markdown>>,
    hide_thinking: bool,
}

async fn execute_interactive_fork(
    runtime: &mut InteractiveRuntime,
    command_name: &str,
    entry_id: String,
    position: ForkPosition,
    context: InteractiveForkContext<'_>,
    lifecycle_request: Option<(
        Arc<ExtensionHostState>,
        crate::core::extensions::types::PendingHostAction,
    )>,
) -> InteractiveForkResult {
    let position_name = match position {
        ForkPosition::Before => "before",
        ForkPosition::At => "at",
    };
    if !session_fork_allowed(runtime, &entry_id, position_name) {
        if let Some((host, request)) = lifecycle_request.as_ref() {
            let _ = host.complete_lifecycle_action(
                request.clone(),
                json!({"cancelled": true, "context": host.snapshot()}),
            );
        }
        return InteractiveForkResult {
            status: "session fork cancelled by extension".to_string(),
            editor_text: None,
        };
    }

    let selected_text = if position == ForkPosition::Before {
        runtime
            .session
            .find_entry(&pi_agent::session::state::EntryQuery {
                id: Some(entry_id.clone()),
                ..Default::default()
            })
            .await
            .ok()
            .flatten()
            .and_then(|entry| match entry {
                pi_agent::session::types::Entry::Message {
                    message: pi_agent::types::AgentMessage::Core(Message::User(user)),
                    ..
                } => Some(pi_agent::agent::user_content_text(&user)),
                _ => None,
            })
    } else {
        None
    };
    let source_metadata = runtime.session.get_metadata().await;
    let previous_session_file = source_metadata.path.clone();
    if runtime.messages.len() > runtime.persisted_until {
        let pending = runtime.messages[runtime.persisted_until..].to_vec();
        if let Err(error) = persist_messages_checked(&mut runtime.session, &pending).await {
            return InteractiveForkResult {
                status: format!("{command_name} failed: {error}"),
                editor_text: None,
            };
        }
        runtime.persisted_until = runtime.messages.len();
    }
    let new_id = pi_agent::session::new_id();
    let fork_result = runtime
        .repo
        .fork(
            &source_metadata,
            CreateOptions {
                id: Some(new_id.clone()),
                cwd: runtime.cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Branch {
                    entry_id: Some(entry_id),
                    position: Some(position),
                },
            },
        )
        .await;
    let mut session = match fork_result {
        Ok(session) => session,
        Err(error) => {
            return InteractiveForkResult {
                status: format!("{command_name} failed: {error}"),
                editor_text: None,
            }
        }
    };
    let target_session_file = session.get_metadata().await.path;

    let (messages, cache_entries) = {
        let runtime_session = &mut session;
        let entries = runtime_session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap_or_default();
        let messages = entries
            .iter()
            .filter_map(|entry| match entry {
                pi_agent::session::types::Entry::Message { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let cache_entries = entries
            .iter()
            .filter_map(|entry| serde_json::to_value(entry).ok())
            .collect::<Vec<_>>();
        (messages, cache_entries)
    };

    if let Some((host, request)) = lifecycle_request {
        let _ = host.complete_lifecycle_action(
            request,
            json!({
                "cancelled": false,
                "result": "fork-created",
                "sessionFile": target_session_file.clone(),
                "sessionId": new_id.clone(),
                "context": {"sessionFile": target_session_file.clone(), "sessionId": new_id.clone()},
            }),
        );
    }
    shutdown_extensions_before_session_replace(runtime, "fork", Some(&target_session_file));

    invalidate_interactive_harness(runtime);
    runtime.session = session;
    runtime.session_id = new_id;
    runtime.session_name = runtime.session.get_name().await;
    runtime.messages = messages;
    runtime.cache_entries = cache_entries;
    runtime.persisted_until = runtime.messages.len();
    context
        .transcript_md
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .set_text(it::compose_transcript(
            &runtime.messages,
            context.hide_thinking,
            "",
        ));
    let notes = replace_extensions(
        runtime,
        context.settings,
        context.thinking_level,
        "fork",
        Some(&previous_session_file),
        Some(&target_session_file),
    );
    let mut status = format!(
        "{command_name} session {} ({} prior messages)",
        runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
        runtime.messages.len()
    );
    if !notes.is_empty() {
        status.push_str(&format!(" (extensions: {})", notes.join("; ")));
    }
    InteractiveForkResult {
        status,
        editor_text: Some(match position {
            ForkPosition::Before => selected_text.unwrap_or_default(),
            ForkPosition::At => String::new(),
        }),
    }
}

/// Consume lifecycle requests emitted by an external extension callback at a
/// non-reentrant turn boundary. The bridge cannot await a Rust mode while its
/// callback is active, so the mode owns the actual session mutation here and
/// reports cancellation or storage failures instead of treating a queued
/// request as a successful no-op.
async fn apply_pending_extension_lifecycle_actions(
    runtime: &mut InteractiveRuntime,
    settings: &mut SettingsManager,
    thinking_level: &str,
    transcript_md: &Arc<Mutex<Markdown>>,
    hide_thinking: bool,
) -> Vec<String> {
    let actions = runtime
        .extensions
        .host
        .drain_pending_lifecycle_action_metadata();
    let mut notes = Vec::new();
    for request in actions {
        let action = request.payload.clone();
        let action_type = action.get("type").and_then(Value::as_str).unwrap_or("");
        let options = action.get("options").unwrap_or(&Value::Null);
        let request_host = runtime.extensions.host.clone();
        if !runtime.session_persistence
            && matches!(action_type, "new_session" | "fork" | "switch_session")
        {
            let message = format!(
                "extension {action_type} requires session persistence; remove --no-session"
            );
            let _ = request_host.complete_lifecycle_action(
                request,
                json!({
                    "cancelled": true,
                    "error": message,
                    "snapshot": request_host.snapshot(),
                }),
            );
            notes.push(message);
            continue;
        }
        let mut completion_sent = false;
        let result: Result<String, String> = match action_type {
            "new_session" => {
                (async {
                    if !session_switch_allowed(runtime, "new", None) {
                        return Err("new session cancelled by extension".to_string());
                    }
                    let previous_session_file = runtime.session.get_metadata().await.path;
                    let new_id = pi_agent::session::new_id();
                    let parent_session_id = options
                        .get("parentSession")
                        .or_else(|| options.get("parentSessionId"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let session = runtime
                        .repo
                        .create(CreateOptions {
                            id: Some(new_id.clone()),
                            cwd: runtime.cwd.clone(),
                            parent_session_id,
                            metadata: None,
                            fork_options: ForkOptions::Tree,
                        })
                        .await
                        .map_err(|error| format!("new session failed: {error}"))?;
                    let target_session_file = session.get_metadata().await.path;
                    let request_host = runtime.extensions.host.clone();
                    // The replacement callback must observe the target
                    // session while the old bridge is still alive. Update the
                    // shared synchronous snapshot before sending completion;
                    // the durable runtime/session swap follows immediately
                    // after the callback acknowledgement.
                    let _ = request_host.dispatch(
                        crate::core::extensions::ExtensionHostAction::SetSessionName,
                        &json!({"name": null}),
                    );
                    let _ = request_host.complete_lifecycle_action(
                        request.clone(),
                        json!({
                            "cancelled": false,
                            "result": "new-session-created",
                            "sessionFile": target_session_file.clone(),
                            "sessionId": new_id.clone(),
                            "context": {"sessionFile": target_session_file.clone(), "sessionId": new_id.clone()},
                            "snapshot": request_host.snapshot(),
                        }),
                    );
                    completion_sent = true;
                    shutdown_extensions_before_session_replace(
                        runtime,
                        "new",
                        Some(&target_session_file),
                    );
                    invalidate_interactive_harness(runtime);
                    runtime.session = session;
                    runtime.session_id = new_id;
                    runtime.session_name = None;
                    runtime.messages.clear();
                    runtime.cache_entries.clear();
                    runtime.persisted_until = 0;
                    transcript_md.lock().unwrap_or_else(|error| error.into_inner()).set_text("");
                    let reload_notes = replace_extensions(
                        runtime,
                        settings,
                        thinking_level,
                        "new",
                        Some(&previous_session_file),
                        Some(&target_session_file),
                    );
                    Ok(format!(
                        "extension started new session {}{}",
                        runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                        if reload_notes.is_empty() {
                            String::new()
                        } else {
                            format!(" (extensions: {})", reload_notes.join("; "))
                        }
                    ))
                })
                .await
            }
            "fork" => {
                (async {
                    let Some(entry_id) = action.get("entryId").and_then(Value::as_str) else {
                        return Err("extension fork missing entryId".to_string());
                    };
                    let position = match options.get("position").and_then(Value::as_str) {
                        Some("before") => ForkPosition::Before,
                        _ => ForkPosition::At,
                    };
                    let fork = execute_interactive_fork(
                        runtime,
                        "extension fork",
                        entry_id.to_string(),
                        position,
                        InteractiveForkContext {
                            settings,
                            thinking_level,
                            transcript_md,
                            hide_thinking,
                        },
                        Some((runtime.extensions.host.clone(), request.clone())),
                    )
                    .await;
                    completion_sent = fork.status.starts_with("extension fork session ");
                    Ok(fork.status)
                })
                .await
            }
            "navigate_tree" => {
                (async {
                    let Some(target_id) = action.get("targetId").and_then(Value::as_str) else {
                        return Err("extension navigateTree missing targetId".to_string());
                    };
                    if !session_switch_allowed(runtime, "navigate", None) {
                        return Err("session navigation cancelled by extension".to_string());
                    }
                    runtime
                        .session
                        .move_lane("main", Some(target_id))
                        .await
                        .map_err(|error| format!("navigateTree failed: {error}"))?;
                    let (messages, cache_entries) =
                        rehydrate_transcript(runtime, transcript_md, hide_thinking).await;
                    runtime.messages = messages;
                    runtime.cache_entries = cache_entries;
                    runtime.persisted_until = runtime.messages.len();
                    let _ = runtime.extensions.host.complete_lifecycle_action(
                        request.clone(),
                        json!({
                            "cancelled": false,
                            "result": "tree-navigated",
                            "targetId": target_id,
                            "snapshot": runtime.extensions.host.snapshot(),
                        }),
                    );
                    completion_sent = true;
                    Ok(format!("extension navigated session tree to {target_id}"))
                })
                .await
            }
            "switch_session" => {
                (async {
                    let Some(selector) = action.get("sessionPath").and_then(Value::as_str) else {
                        return Err("extension switchSession missing sessionPath".to_string());
                    };
                    let metadata = crate::run::resolve_session_metadata(
                        &runtime.repo,
                        selector,
                        &runtime.cwd,
                    )
                        .await
                        .map_err(|error| format!("switchSession failed: {error}"))?;
                    if !session_switch_allowed(runtime, "switch", Some(&metadata.path)) {
                        return Err("session switch cancelled by extension".to_string());
                    }
                    let previous_session_file = runtime.session.get_metadata().await.path;
                    let session = runtime
                        .repo
                        .open(&metadata)
                        .await
                        .map_err(|error| format!("switchSession failed: {error}"))?;
                    let target_session_file = session.get_metadata().await.path;
                    let target_session_name = session.get_name().await;
                    let request_host = runtime.extensions.host.clone();
                    let _ = request_host.dispatch(
                        crate::core::extensions::ExtensionHostAction::SetSessionName,
                        &json!({"name": target_session_name}),
                    );
                    let _ = request_host.complete_lifecycle_action(
                        request.clone(),
                        json!({
                            "cancelled": false,
                            "result": "session-switched",
                            "sessionFile": target_session_file.clone(),
                            "sessionId": metadata.id.clone(),
                            "context": {"sessionFile": target_session_file.clone(), "sessionId": metadata.id.clone()},
                            "snapshot": request_host.snapshot(),
                        }),
                    );
                    completion_sent = true;
                    shutdown_extensions_before_session_replace(
                        runtime,
                        "switch",
                        Some(&target_session_file),
                    );
                    invalidate_interactive_harness(runtime);
                    runtime.session = session;
                    runtime.session_id = metadata.id.clone();
                    runtime.session_name = runtime.session.get_name().await;
                    let (messages, cache_entries) =
                        rehydrate_transcript(runtime, transcript_md, hide_thinking).await;
                    runtime.messages = messages;
                    runtime.cache_entries = cache_entries;
                    runtime.persisted_until = runtime.messages.len();
                    let reload_notes = replace_extensions(
                        runtime,
                        settings,
                        thinking_level,
                        "switch",
                        Some(&previous_session_file),
                        Some(&target_session_file),
                    );
                    Ok(format!(
                        "extension switched to session {}{}",
                        runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                        if reload_notes.is_empty() {
                            String::new()
                        } else {
                            format!(" (extensions: {})", reload_notes.join("; "))
                        }
                    ))
                })
                .await
            }
            "reload" => {
                (async {
                    settings.reload().await;
                    refresh_interactive_retry_settings(runtime, settings);
                    let mut reload_notes = settings
                        .drain_errors()
                        .into_iter()
                        .map(|error| format!("settings: {}", error.error))
                        .collect::<Vec<_>>();
                    let _ = request_host.complete_lifecycle_action(
                        request.clone(),
                        json!({
                            "cancelled": false,
                            "result": "reload-started",
                            "context": {"reason": "reload"},
                            "snapshot": request_host.snapshot(),
                        }),
                    );
                    completion_sent = true;
                    reload_notes.extend(reload_extensions(runtime, settings, thinking_level));
                    reload_notes.extend(reload_interactive_models(runtime));
                    register_interactive_themes(
                        &runtime.extension_args,
                        settings,
                        &runtime.extension_resources,
                        &runtime.cwd,
                    );
                    if let Some(theme_name) = settings.get_theme_setting() {
                        load_interactive_theme_setting(theme_name);
                    }
                    Ok(if reload_notes.is_empty() {
                        "extension reloaded settings and runtime".to_string()
                    } else {
                        format!("extension reload: {}", reload_notes.join("; "))
                    })
                })
                .await
            }
            _ => Err(format!(
                "unsupported extension lifecycle action: {action_type}"
            )),
        };
        if !completion_sent {
            let completion = match &result {
                Ok(message) => json!({
                    "cancelled": message.contains("cancelled"),
                    "result": message,
                    "snapshot": request_host.snapshot(),
                }),
                Err(error) => json!({
                    "cancelled": error.contains("cancelled"),
                    "error": error,
                    "snapshot": request_host.snapshot(),
                }),
            };
            if let Err(error) = request_host.complete_lifecycle_action(request, completion) {
                tracing::warn!(%error, "failed to complete interactive extension lifecycle action");
            }
        }
        match result {
            Ok(note) => notes.push(note),
            Err(error) => notes.push(error),
        }
    }
    notes
}

/// Apply host mutations queued by an extension callback at the boundary before
/// constructing the next real interactive turn. Requests are drained
/// atomically so a model and tool policy from the same callback are applied to
/// the same turn.
fn apply_extension_turn_changes(runtime: &mut InteractiveRuntime) {
    let changes = runtime.extensions.host.drain_requested_changes();
    if let Some(model_value) = changes.model {
        let provider = model_value.get("provider").and_then(Value::as_str);
        let model_id = model_value.get("id").and_then(Value::as_str);
        if let (Some(provider), Some(model_id)) = (provider, model_id) {
            if let Some(model) = runtime.models.get_model(provider, model_id) {
                invalidate_interactive_harness(runtime);
                runtime.provider = provider.to_string();
                runtime.model = model.clone();
                runtime.extensions.host.set_model(Some(model_value));
            } else {
                tracing::warn!(%provider, %model_id, "extension requested an unavailable interactive model");
            }
        }
    }
    if let Some(active_tools) = changes.active_tools {
        invalidate_interactive_harness(runtime);
        runtime.active_tool_names = Some(active_tools);
    }
}

/// Cycle the active model through the explicit scoped-models set. Pi uses
/// Ctrl+P for this operation; an empty set intentionally leaves the normal
/// model selector behavior unchanged.
fn cycle_scoped_model(runtime: &mut InteractiveRuntime) -> Option<String> {
    if runtime.scoped_models.len() < 2 {
        return None;
    }
    let current = format!("{}/{}", runtime.provider, runtime.model.id);
    let current_index = runtime
        .scoped_models
        .iter()
        .position(|reference| reference.eq_ignore_ascii_case(&current))
        .unwrap_or(0);
    let next_reference =
        runtime.scoped_models[(current_index + 1) % runtime.scoped_models.len()].clone();
    let (provider, model_id) = next_reference.split_once('/')?;
    let model = runtime.models.get_model(provider, model_id)?.clone();
    invalidate_interactive_harness(runtime);
    runtime.provider = provider.to_string();
    runtime.model = model;
    Some(format!("Model: {next_reference}"))
}

/// Build the startup notice for an explicit model scope. Upstream emits this
/// before entering the TUI whenever startup presentation is enabled, so a
/// user can see both the scoped IDs and their per-model thinking overrides.
fn model_scope_startup_message(
    scoped_models: &[crate::core::model_resolver::ScopedModel],
    verbose: bool,
    quiet_startup: bool,
) -> Option<String> {
    if scoped_models.is_empty() || (!verbose && quiet_startup) {
        return None;
    }
    let model_list = scoped_models
        .iter()
        .map(|scoped| {
            let thinking = scoped
                .thinking_level
                .as_deref()
                .filter(|level| !level.is_empty())
                .map(|level| format!(":{level}"))
                .unwrap_or_default();
            format!("{}{}", scoped.model.id, thinking)
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("Model scope: {model_list} (Ctrl+P to cycle)"))
}

fn apply_model_reference(runtime: &mut InteractiveRuntime, value: &str) -> Result<String, String> {
    let (provider, model_id) = it::parse_model_selection(value)
        .ok_or_else(|| "usage: /model <provider/model>".to_string())?;
    let model = runtime
        .models
        .get_model(&provider, &model_id)
        .ok_or_else(|| format!("model not found: {value}"))?;
    invalidate_interactive_harness(runtime);
    runtime.provider = provider;
    runtime.model = model;
    Ok(format!("Model: {}", runtime.model.id))
}

fn maybe_add_daxnuts_component(
    runtime: &InteractiveRuntime,
    easter_egg_components: &mut Vec<SharedComponent>,
    animation_until: &mut Option<std::time::Instant>,
) -> bool {
    if it::easter_eggs::is_daxnuts_model(&runtime.provider, &runtime.model.id) {
        easter_egg_components.push(it::easter_eggs::daxnuts_component());
        *animation_until =
            Some(std::time::Instant::now() + it::easter_eggs::daxnuts_animation_duration());
        true
    } else {
        false
    }
}

fn clear_easter_egg_components(
    easter_egg_components: &mut Vec<SharedComponent>,
    animation_until: &mut Option<std::time::Instant>,
) {
    easter_egg_components.clear();
    *animation_until = None;
}

fn debug_render_lines(
    transcript_md: &Arc<Mutex<Markdown>>,
    editor: &Arc<Mutex<Editor>>,
    footer_text: &Arc<Mutex<Text>>,
    easter_egg_components: &[SharedComponent],
    width: usize,
) -> Vec<String> {
    let mut lines = transcript_md
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .render(width);
    for component in easter_egg_components {
        lines.extend(
            component
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .render(width),
        );
    }
    lines.extend(
        editor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(width),
    );
    lines.extend(
        footer_text
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render(width),
    );
    lines
}

fn iso_timestamp_now() -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        elapsed.subsec_millis()
    )
}

// Howard Hinnant's civil-date conversion, kept local so /debug does not add a
// date/time dependency to the interactive runtime.
fn civil_date_from_unix_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn write_debug_snapshot(
    path: &std::path::Path,
    width: usize,
    height: usize,
    lines: &[String],
    messages: &[pi_agent::types::AgentMessage],
) -> Result<(), String> {
    let timestamp = iso_timestamp_now();
    let mut data = format!(
        "Debug output at {timestamp}\nTerminal: {width}x{height}\nTotal lines: {}\n\n",
        lines.len(),
    );
    data.push_str("=== All rendered lines with visible widths ===\n");
    for (index, line) in lines.iter().enumerate() {
        data.push_str(&format!(
            "[{index}] (w={}) {}\n",
            pi_tui::utils::visible_width(line),
            serde_json::to_string(line).unwrap_or_else(|_| "\"<invalid>\"".to_string()),
        ));
    }
    data.push_str("\n=== Agent messages (JSONL) ===\n");
    for message in messages {
        data.push_str(
            &serde_json::to_string(message)
                .unwrap_or_else(|_| "{\"error\":\"message serialization failed\"}".to_string()),
        );
        data.push('\n');
    }
    data.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("create parent: {error}"))?;
    }
    std::fs::write(path, data).map_err(|error| format!("write {}: {error}", path.display()))
}

struct InteractiveIdleGuard {
    host: Arc<ExtensionHostState>,
}

impl InteractiveIdleGuard {
    fn new(host: Arc<ExtensionHostState>) -> Self {
        host.set_idle(false);
        Self { host }
    }
}

impl Drop for InteractiveIdleGuard {
    fn drop(&mut self) {
        self.host.set_idle(true);
    }
}

/// Provider errors that have an assistant message are already rendered by
/// `render_assistant_message` in the transcript. Only an infrastructure/task
/// failure without that message belongs in the transient status slot; copying
/// `AssistantMessage::error_message` here duplicates the visible error below
/// the transcript, unlike Pi's assistant component.
fn interactive_turn_error_banner(
    result: &Result<Vec<pi_agent::types::AgentMessage>, String>,
) -> Option<String> {
    result.as_ref().err().cloned()
}

/// Stream a prompt through the agent loop, observing raw events.
struct InteractiveTurnWorker {
    agent: Arc<pi_agent::rich_agent::Agent>,
    task: tokio::task::JoinHandle<Result<InteractiveTurnResult, String>>,
    _idle_guard: InteractiveIdleGuard,
}

/// Result of one harness-backed interactive turn. The returned message delta
/// drives the caller's event/queue behavior, while the exact session-entry
/// delta carries compaction boundaries and their durable metadata across the
/// in-memory worker boundary.
struct InteractiveTurnResult {
    messages: Vec<pi_agent::types::AgentMessage>,
    entries: Vec<Entry>,
    active_messages: Vec<pi_agent::types::AgentMessage>,
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
async fn start_interactive_turn(
    runtime: &mut InteractiveRuntime,
    message: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
    on_tool_event: Arc<dyn Fn(&RichAgentEvent) + Send + Sync>,
    steering_mode: Option<pi_agent::harness::agent_harness::QueueMode>,
    follow_up_mode: Option<pi_agent::harness::agent_harness::QueueMode>,
    session_environment: Option<&crate::core::session_env::SessionEnvironmentGuard>,
) -> Result<InteractiveTurnWorker, String> {
    apply_extension_turn_changes(runtime);
    if let Some(environment) = session_environment {
        environment.set_model(&runtime.provider, &runtime.model.name);
    }
    let idle_guard = InteractiveIdleGuard::new(runtime.extensions.host.clone());
    let prompt = pi_agent::agent::user_text_prompt(message.clone(), pi_ai::types::now_ms());
    let tools = interactive_turn_tools(runtime);
    let models = runtime.models.clone();
    let api_key = config::nonempty_env_value(std::env::var(config::ENV_KEY).ok());
    let stream_options = pi_ai::types::StreamOptions {
        base: pi_ai::types::ProviderRequestOptions {
            api_key,
            // Pi treats a zero idle timeout as disabled by passing the
            // largest representable SDK timeout. Provider adapters then
            // interpret it as effectively unbounded rather than an
            // immediate timeout.
            timeout_ms: Some(runtime.provider_timeout_ms.unwrap_or({
                if runtime.http_idle_timeout_ms == 0 {
                    i32::MAX as u64
                } else {
                    runtime.http_idle_timeout_ms
                }
            })),
            max_retries: runtime.provider_max_retries,
            max_retry_delay_ms: Some(runtime.max_retry_delay_ms),
            ..Default::default()
        },
        transport: Some(runtime.transport.clone()),
        websocket_connect_timeout_ms: runtime.websocket_connect_timeout_ms,
        ..Default::default()
    };
    let harness_stream_options = stream_options.clone();
    let provider = runtime.provider.clone();
    if provider != "faux" {
        crate::core::model_runtime::refresh_provider_oauth_if_needed(&models, &provider).await?;
    }
    let provider_uses_oauth = models
        .get_provider(&provider)
        .is_some_and(|registered| registered.auth.oauth.is_some());
    let stream_fn: crate::run::StreamFn = if provider == "faux" {
        let core = runtime.faux_core.clone().unwrap_or_else(|| {
            crate::core::model_runtime::register_faux_provider(
                &models,
                &pi_ai::providers::RegisterFauxProviderOptions::default(),
            )
        });
        // The first response is tied to this submitted prompt. Additional
        // responses are factories so steering/follow-up input typed while a
        // faux turn is still streaming receives a real second response
        // instead of exhausting the one-step fixture. This keeps the PTY
        // harness faithful to a provider that can answer every queued turn;
        // production providers never use this branch.
        let mut responses = vec![pi_ai::providers::FauxResponseStep::Message(
            pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(format!(
                    "faux response to: {message}"
                ))],
                pi_ai::providers::FauxAssistantOptions::default(),
            ),
        )];
        for _ in 0..32 {
            responses.push(pi_ai::providers::FauxResponseStep::Factory(Box::new(
                |context: &pi_ai::types::Context,
                 _options: Option<&pi_ai::types::SimpleStreamOptions>,
                 _state: &pi_ai::providers::FauxProviderState,
                 _model: &pi_ai::model::Model| {
                    let prompt = context
                        .messages
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            pi_ai::types::Message::User(user) => {
                                Some(pi_agent::agent::user_content_text(user))
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    pi_ai::providers::faux_assistant_message(
                        vec![pi_ai::types::ContentBlock::text(format!(
                            "faux response to: {prompt}"
                        ))],
                        pi_ai::providers::FauxAssistantOptions::default(),
                    )
                },
            )));
        }
        core.set_responses(responses);
        let stream_models = models.clone();
        let faux_stream_options = stream_options.clone();
        Arc::new(move |model, ctx| stream_models.stream(model, ctx, Some(&faux_stream_options)))
    } else {
        Arc::new(move |model, ctx| models.stream(model, ctx, Some(&stream_options)))
    };
    let (harness, baseline_entry_ids) = if let Some(harness) = runtime.interactive_harness.clone() {
        let baseline_entry_ids: std::collections::HashSet<String> = harness
            .transcript()
            .await
            .map_err(|error| format!("interactive turn: read live harness transcript: {error}"))?
            .into_iter()
            .map(|entry| entry.id().to_string())
            .collect();
        (harness, baseline_entry_ids)
    } else {
        let storage = Arc::new(Mutex::new(
            pi_agent::session::memory::InMemorySessionStorage::new(
                pi_agent::session::memory::in_memory_metadata("interactive-turn", None),
            ),
        ));
        let mut session =
            pi_agent::session::Session::<pi_agent::fs::MemoryFs>::from_in_memory(storage);
        let seed_entries = runtime
            .session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                id: None,
                entry_type: None,
                custom_type: None,
                cursor: None,
                limit: None,
            })
            .await
            .map_err(|error| format!("interactive turn: read session entries: {error}"))?;
        let baseline_entry_ids = seed_entries
            .iter()
            .map(|entry| entry.id().to_string())
            .collect();
        for entry in &seed_entries {
            session
                .append_entry(entry.to_no_stats(), "main")
                .await
                .map_err(|error| format!("interactive turn: seed session: {error}"))?;
        }
        let mut options = AgentHarnessOptions::new(session, runtime.model.clone());
        options.stream_fn = Some(stream_fn);
        options.system_prompt = runtime.system_prompt.clone();
        options.block_images = runtime.block_images;
        options.tool_result_image_options = Some(pi_agent::tools::image::ProcessImageOptions {
            auto_resize_images: runtime.auto_resize_images,
            ..Default::default()
        });
        options.compaction = Some(runtime.compaction_settings.clone());
        options.tools = Some(tools.iter().map(HarnessTool::from_agent_tool).collect());
        options.steering_mode = steering_mode;
        options.follow_up_mode = follow_up_mode;
        options.retry = Some(runtime.retry_policy.clone());
        options.stream_options = Some(pi_ai::types::SimpleStreamOptions {
            base: harness_stream_options,
            ..Default::default()
        });
        let (harness, _suspended) = AgentHarness::create(options)
            .await
            .map_err(|error| error.to_string())?;
        let harness = Arc::new(harness);
        runtime.interactive_harness = Some(harness.clone());
        (harness, baseline_entry_ids)
    };
    let agent = harness
        .agent_handle()
        .ok_or_else(|| "interactive harness has no agent".to_string())?;
    if let Some(mode) = steering_mode {
        agent.set_steering_mode(match mode {
            pi_agent::harness::agent_harness::QueueMode::All => {
                pi_agent::rich_agent::QueueMode::All
            }
            pi_agent::harness::agent_harness::QueueMode::OneAtATime => {
                pi_agent::rich_agent::QueueMode::OneAtATime
            }
        });
    }
    if let Some(mode) = follow_up_mode {
        agent.set_follow_up_mode(match mode {
            pi_agent::harness::agent_harness::QueueMode::All => {
                pi_agent::rich_agent::QueueMode::All
            }
            pi_agent::harness::agent_harness::QueueMode::OneAtATime => {
                pi_agent::rich_agent::QueueMode::OneAtATime
            }
        });
    }

    // Install one live listener for the retained harness. The callback slots
    // are replaced per turn so an interactive session can make arbitrarily
    // many prompts without rendering each stream event once per historical
    // turn. Tool lifecycle events share this listener with assistant stream
    // events, but have their own slot so the public assistant callback stays
    // source-compatible.
    if runtime.interactive_event_handler.is_none() {
        let handler: InteractiveEventHandler = Arc::new(Mutex::new(None));
        let tool_handler: InteractiveToolEventHandler = Arc::new(Mutex::new(None));
        let handler_for_listener = handler.clone();
        let tool_handler_for_listener = tool_handler.clone();
        let listener_provider = provider.clone();
        let faux_stream_delay = (listener_provider == "faux")
            .then(|| {
                std::env::var("PI_RUST_INTERACTIVE_FAUX_STREAM_DELAY_MS")
                    .ok()?
                    .parse::<u64>()
                    .ok()
                    .filter(|delay_ms| *delay_ms > 0)
                    .map(std::time::Duration::from_millis)
            })
            .flatten();
        let _ = agent.subscribe(move |mut event, _signal| {
            let handler = handler_for_listener.clone();
            let tool_handler = tool_handler_for_listener.clone();
            let provider = listener_provider.clone();
            Box::pin(async move {
                match &mut event {
                    RichAgentEvent::MessageUpdate {
                        assistant_message_event,
                        ..
                    } => {
                        if let AssistantMessageEvent::Error { error_message, .. } =
                            assistant_message_event
                        {
                            crate::core::auth_guidance::rewrite_assistant_error(
                                error_message,
                                &provider,
                                provider_uses_oauth,
                            );
                        }
                        let callback = handler
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .as_ref()
                            .cloned();
                        if let Some(callback) = callback {
                            callback(assistant_message_event);
                        }
                    }
                    RichAgentEvent::ToolExecutionStart { .. }
                    | RichAgentEvent::ToolExecutionUpdate { .. }
                    | RichAgentEvent::ToolExecutionEnd { .. } => {
                        let callback = tool_handler
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .as_ref()
                            .cloned();
                        if let Some(callback) = callback {
                            callback(&event);
                        }
                    }
                    _ => {}
                }
                // Let the interactive turn own the next ready PTY event
                // between streamed lifecycle notifications. Provider
                // streams can deliver a complete faux/offline response in a
                // single executor slice; yielding here keeps steering,
                // follow-up, resize, and cancellation input observable while
                // the assistant delta stream is still active.
                if let Some(delay) = faux_stream_delay {
                    // The deterministic faux provider resolves each token on
                    // a microtask, unlike a network stream. The PTY harness
                    // may opt into a small real-time cadence so it can observe
                    // input ownership while a faux stream is active.
                    tokio::time::sleep(delay).await;
                } else {
                    tokio::task::yield_now().await;
                }
            })
        });
        runtime.interactive_event_handler = Some(handler.clone());
        runtime.interactive_tool_event_handler = Some(tool_handler);
    }
    let event_handler = runtime
        .interactive_event_handler
        .clone()
        .expect("interactive event handler installed");
    let tool_event_handler = runtime
        .interactive_tool_event_handler
        .clone()
        .expect("interactive tool event handler installed");
    *event_handler
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(on_event);
    *tool_event_handler
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(on_tool_event);

    let task_harness = Arc::clone(&harness);
    let task_event_handler = event_handler.clone();
    let task_tool_event_handler = tool_event_handler.clone();
    let task_provider = provider.clone();
    let task = tokio::spawn(async move {
        let run_result = task_harness
            .run_prompt(vec![prompt])
            .await
            .map_err(|error| error.to_string());
        *task_event_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        *task_tool_event_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let mut new_messages = run_result?;
        for message in &mut new_messages {
            if let pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) = message {
                crate::core::auth_guidance::rewrite_assistant_error(
                    assistant,
                    &task_provider,
                    provider_uses_oauth,
                );
            }
        }
        let active_messages = task_harness
            .agent_messages()
            .await
            .map_err(|error| error.to_string())?;
        let entries = task_harness
            .transcript()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|entry| !baseline_entry_ids.contains(entry.id()))
            .collect();
        Ok(InteractiveTurnResult {
            messages: new_messages,
            entries,
            active_messages,
        })
    });
    Ok(InteractiveTurnWorker {
        agent,
        task,
        _idle_guard: idle_guard,
    })
}

async fn finish_interactive_turn(
    runtime: &mut InteractiveRuntime,
    result: InteractiveTurnResult,
) -> Result<Vec<pi_agent::types::AgentMessage>, String> {
    persist_entries_checked(&mut runtime.session, &result.entries).await?;
    runtime.messages = result.active_messages;
    let entries = runtime
        .session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            id: None,
            entry_type: None,
            custom_type: None,
            cursor: None,
            limit: None,
        })
        .await
        .map_err(|error| format!("refresh interactive session shadow: {error}"))?;
    runtime.cache_entries = entries
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    runtime.persisted_until = runtime.messages.len();
    Ok(result.messages)
}

#[cfg_attr(not(test), allow(dead_code))]
async fn stream_turn(
    runtime: &mut InteractiveRuntime,
    message: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
) -> Result<Vec<pi_agent::types::AgentMessage>, String> {
    let worker = start_interactive_turn(
        runtime,
        message,
        on_event,
        Arc::new(|_| {}),
        None,
        None,
        None,
    )
    .await?;
    let result = worker.task.await.map_err(|error| error.to_string())??;
    finish_interactive_turn(runtime, result).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveQueueKind {
    Steering,
    FollowUp,
}

/// The upstream interactive mode uses the same working-status label for the
/// initial prompt and for a queued follow-up.  The queue count is displayed
/// separately after input is accepted; it is not part of the active loader
/// message.
fn interactive_working_message(_kind: InteractiveQueueKind) -> &'static str {
    "Working..."
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractivePendingMessage {
    text: String,
    kind: InteractiveQueueKind,
}

struct InteractiveStreamingUi<'a> {
    renderer: &'a mut InteractiveRenderer,
    editor: &'a Arc<Mutex<Editor>>,
    transcript_md: &'a Arc<Mutex<Markdown>>,
    transcript_view: &'a Arc<Mutex<InteractiveTranscriptView>>,
    transcript_scroll_view: &'a Arc<Mutex<ScrollView>>,
    status_text: &'a Arc<Mutex<Text>>,
    status_log: &'a InteractiveStatusLog,
    footer_text: &'a Arc<Mutex<Text>>,
    pending_loader: &'a Arc<Mutex<pi_tui::components::Loader>>,
    stream_buffer: &'a Arc<Mutex<String>>,
    live_transcript: &'a Arc<Mutex<InteractiveLiveTranscript>>,
    pending_text: &'a mut String,
    status_banner: &'a mut String,
    hide_thinking: bool,
    show_images: bool,
    image_width_cells: usize,
    output_pad: usize,
    tool_output_expanded: &'a AtomicBool,
    last_projection_key: Option<(
        usize,
        String,
        u64,
        Option<String>,
        it::messages::TranscriptRenderOptions,
    )>,
    cached_scene_pending: Option<String>,
    cached_scene: Option<Arc<Mutex<Scene>>>,
}

impl InteractiveStreamingUi<'_> {
    fn render_options(&self) -> it::messages::TranscriptRenderOptions {
        it::messages::TranscriptRenderOptions {
            hide_thinking: self.hide_thinking,
            show_images: self.show_images,
            image_width_cells: self.image_width_cells,
            output_pad: self.output_pad,
            expand_tool_output: self.tool_output_expanded.load(Ordering::Acquire),
        }
    }

    fn refresh_live_transcript(&mut self) {
        let options = self.render_options();
        let rendered = {
            let mut live = self
                .live_transcript
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            live.configure(options);
            live.render()
        };
        *self
            .stream_buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = rendered;
    }

    fn toggle_tool_output(&mut self) {
        let expanded = !self.tool_output_expanded.fetch_xor(true, Ordering::AcqRel);
        self.refresh_live_transcript();
        *self.status_banner = format!(
            "Tool output: {}",
            if expanded { "expanded" } else { "collapsed" }
        );
        self.renderer.invalidate();
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn render(&mut self, snapshot_messages: &[pi_agent::types::AgentMessage]) {
        self.refresh_live_transcript();
        let stream = self
            .stream_buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let options = self.render_options();
        let projection_key = (
            snapshot_messages.len(),
            stream.clone(),
            self.status_log.revision(),
            self.status_log.active_message().map(str::to_string),
            options,
        );
        if self.last_projection_key.as_ref() != Some(&projection_key) {
            let blocks = build_interactive_transcript_blocks(
                snapshot_messages,
                options,
                &stream,
                &[],
                self.status_log,
            );
            let text = transcript_source_from_blocks(&blocks);
            let mut transcript = self
                .transcript_md
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if transcript.text() != text {
                transcript.set_text(text);
            }
            drop(transcript);
            self.transcript_view
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_blocks(blocks);
            self.status_text
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .set_text(
                    self.status_log
                        .active_message()
                        .map(|message| it::tui_theme::fg("muted", message))
                        .unwrap_or_default(),
                );
            self.last_projection_key = Some(projection_key);
        }
        if self.cached_scene_pending.as_deref() != Some(self.pending_text.as_str()) {
            self.cached_scene = Some(it::build_interactive_scene_with_loader_and_scroll_view(
                self.transcript_scroll_view,
                self.editor,
                self.footer_text,
                Some(self.status_text),
                None,
                &[],
                self.pending_loader,
                self.pending_text,
            ));
            self.cached_scene_pending = Some(self.pending_text.clone());
        }
        let scene = self
            .cached_scene
            .as_ref()
            .expect("streaming scene cache initialized")
            .clone();
        self.renderer.render_scene(&scene);
    }
}

struct InteractiveTurnInput<'a> {
    input: &'a mut InteractiveInputReader,
    steering_mode: &'a str,
    follow_up_mode: &'a str,
    session_environment: Option<&'a crate::core::session_env::SessionEnvironmentGuard>,
    #[cfg(unix)]
    sigcont: &'a mut tokio::signal::unix::Signal,
    #[cfg(unix)]
    use_alt_screen: bool,
}

/// Run an interactive turn while continuing to consume terminal input.
///
/// The old interactive loop awaited `stream_turn` before reading another key,
/// which made upstream steering/follow-up behavior impossible. This helper
/// keeps the terminal event poll live, queues submitted text by behavior, and
/// returns the queue to the caller for delivery at the next turn boundary.
async fn stream_turn_with_input(
    runtime: &mut InteractiveRuntime,
    prompt: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
    on_tool_event: Arc<dyn Fn(&RichAgentEvent) + Send + Sync>,
    ui: &mut InteractiveStreamingUi<'_>,
    turn_input: InteractiveTurnInput<'_>,
) -> (
    Result<Vec<pi_agent::types::AgentMessage>, String>,
    Vec<InteractivePendingMessage>,
) {
    let snapshot_messages = runtime.messages.clone();
    let worker = match start_interactive_turn(
        runtime,
        prompt,
        on_event,
        on_tool_event,
        Some(if turn_input.steering_mode == "all" {
            pi_agent::harness::agent_harness::QueueMode::All
        } else {
            pi_agent::harness::agent_harness::QueueMode::OneAtATime
        }),
        Some(if turn_input.follow_up_mode == "all" {
            pi_agent::harness::agent_harness::QueueMode::All
        } else {
            pi_agent::harness::agent_harness::QueueMode::OneAtATime
        }),
        turn_input.session_environment,
    )
    .await
    {
        Ok(worker) => worker,
        Err(error) => return (Err(error), Vec::new()),
    };
    let InteractiveTurnWorker {
        agent,
        task,
        _idle_guard,
    } = worker;
    let mut turn = Box::pin(task);
    let mut queued = Vec::new();
    let mut redraw = tokio::time::interval(TUI_MIN_RENDER_INTERVAL);
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // A PTY can already have delivered the next key while the
            // provider is settling its terminal event. Preserve the
            // interactive loop's input-first ordering so a follow-up or
            // cancellation is not handed to the idle composer merely because
            // both futures became ready in the same executor turn.
            biased;
            event = turn_input.input.recv() => {
                let event = match event {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => return (Err(error), queued),
                    None => return (Err("terminal input reader stopped".to_string()), queued),
                };
                match event {
                    pi_tui::terminal::TerminalEvent::Resize(_, height) => {
                        ui.renderer.invalidate();
                        ui.editor
                            .lock().unwrap_or_else(|error| error.into_inner())
                            .set_terminal_rows(height as usize);
                    }
                    pi_tui::terminal::TerminalEvent::Key(key_str) => {
                        if key_str.is_empty() {
                            ui.render(&snapshot_messages);
                            continue;
                        }
                        if ui.renderer.consume_cell_size_response(&key_str) {
                            continue;
                        }
                        // Kitty emits a press and release CSI-u event. The
                        // interactive handlers act on presses; dispatching
                        // the release would apply navigation a second time.
                        if is_key_release(&key_str) {
                            continue;
                        }
                        let key = parse_key(&key_str);
                        if key.ctrl && key.base == "z" && !key.alt && !key.shift {
                            #[cfg(unix)]
                            {
                                let terminal = ui.renderer.terminal_handle();
                                let result = suspend_interactive(
                                    &terminal,
                                    turn_input.input,
                                    turn_input.use_alt_screen,
                                    turn_input.sigcont,
                                )
                                .await;
                                match result {
                                    Ok(()) => {
                                        ui.renderer.invalidate();
                                        ui.render(&snapshot_messages);
                                    }
                                    Err(error) => return (Err(error), queued),
                                }
                            }
                            #[cfg(not(unix))]
                            {
                                // Match upstream: the Windows/non-Unix TUI
                                // has no process-group suspend operation.
                                *ui.pending_text =
                                    "Suspend to background is not supported on Windows".to_string();
                                ui.render(&snapshot_messages);
                            }
                            continue;
                        }
                        if key.base == "esc"
                            || key.base == "escape"
                            || (key.ctrl && key.base == "c")
                        {
                            // Pi restores both submitted queue entries and the
                            // current draft before aborting. Clear the agent's
                            // own queues as well: otherwise its next prompt
                            // would replay the same messages that are now back
                            // in the editor.
                            agent.clear_all_queues();
                            agent.abort();
                            {
                                let mut editor = ui.editor.lock().unwrap_or_else(|error| error.into_inner());
                                restore_interactive_queued_input(&mut editor, &queued);
                            }
                            let result = match turn.await {
                                Ok(Ok(result)) => finish_interactive_turn(runtime, result).await,
                                Ok(Err(error)) => Err(error),
                                Err(error) => Err(error.to_string()),
                            };
                            return (result, Vec::new());
                        }
                        if key.ctrl && key.base == "o" && !key.alt && !key.shift {
                            ui.toggle_tool_output();
                            ui.render(&snapshot_messages);
                            continue;
                        }

                        // When Kitty is not active, terminals commonly send
                        // Return as LF. The shared Editor treats a literal LF
                        // as Shift+Enter for Kitty/Ghostty mappings, so pass
                        // the canonical key name at this mode boundary when
                        // the parsed key is an ordinary Enter. This preserves
                        // multiline input while making legacy Return submit
                        // exactly like the upstream `matchesKey` path.
                        let editor_input = interactive_editor_input(&key_str, &key);
                        let queued_text = if is_streaming_follow_up_key(&key_str, &key) {
                            let text = ui.editor.lock().unwrap_or_else(|error| error.into_inner()).get_text();
                            ui.editor.lock().unwrap_or_else(|error| error.into_inner()).set_text("");
                            Some((text, InteractiveQueueKind::FollowUp))
                        } else if key.base == "enter" && !key.alt && !key.ctrl {
                            let mut guard = ui.editor.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle_input(editor_input);
                            guard
                                .drain_submitted()
                                .map(|text| (text, InteractiveQueueKind::Steering))
                        } else {
                            let mut editor = ui.editor.lock().unwrap_or_else(|error| error.into_inner());
                            if is_printable_input_batch(&key_str, &key) {
                                insert_interactive_text_batch(&mut editor, &key_str);
                            } else {
                                editor.handle_input(editor_input);
                            }
                            None
                        };

                        if let Some((text, kind)) = queued_text {
                            if !text.trim().is_empty() {
                                ui.editor.lock().unwrap_or_else(|error| error.into_inner()).add_to_history(&text);
                                let expanded_text = it::expand_skill_command(&text, &runtime.skills);
                                let queued_message = pi_agent::agent::user_text_prompt(
                                    expanded_text,
                                    pi_ai::types::now_ms(),
                                );
                                match kind {
                                    InteractiveQueueKind::Steering => agent.steer(queued_message),
                                    InteractiveQueueKind::FollowUp => agent.follow_up(queued_message),
                                }
                                queued.push(InteractivePendingMessage { text, kind });
                                *ui.pending_text = format!(" … {} queued", queued.len());
                            }
                        }
                        ui.render(&snapshot_messages);
                    }
                }
            }
            result = &mut turn => {
                let result = match result {
                    Ok(Ok(result)) => finish_interactive_turn(runtime, result).await,
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.to_string()),
                };
                return (result, Vec::new());
            }
            _ = redraw.tick() => {
                ui.render(&snapshot_messages);
            }
        }
    }
}

/// Run the shared interactive compaction path. Automatic compaction observes
/// the threshold; `/compact` forces the same persistence/context replacement
/// path and may provide custom summarization instructions.
async fn compact_interactive(
    runtime: &mut InteractiveRuntime,
    settings_manager: &SettingsManager,
    custom_instructions: Option<&str>,
    force: bool,
) -> Result<bool, String> {
    let operation = if force { "compact" } else { "auto-compact" };
    let settings = interactive_compaction_settings(settings_manager);
    if force {
        // Upstream aborts before it reads or prepares the branch. Automatic
        // threshold compaction runs as part of the turn and must not do so.
        if let Some(harness) = runtime.interactive_harness.as_ref() {
            match harness.abort().await {
                Ok(_) => {}
                Err(HarnessError::Tagged(error)) if error.tag == "NoActiveOperation" => {}
                Err(error) => return Err(format!("{operation}: abort active operation: {error}")),
            }
        }
    }
    if !force {
        let estimate = pi_agent::harness::compaction::estimate_context_tokens(&runtime.messages);
        if !pi_agent::harness::compaction::should_compact(
            estimate.tokens,
            runtime.model.context_window,
            &settings,
        ) {
            return Ok(false);
        }
    }
    let entries = runtime
        .session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            id: None,
            entry_type: None,
            custom_type: None,
            cursor: None,
            limit: None,
        })
        .await
        .map_err(|e| format!("{operation}: read entries: {e}"))?;
    let Some(preparation) = pi_agent::harness::compaction::prepare_compaction(&entries, &settings)
        .map_err(|e| format!("{operation}: prepare: {e}"))?
    else {
        return Ok(false);
    };
    let first_kept_entry_id = preparation.retained_tail.first().and_then(|kept| {
        entries.iter().find_map(|entry| {
            entry
                .as_message()
                .filter(|message| *message == kept)
                .map(|_| entry.id().to_string())
        })
    });
    let preparation_value = json!({
        "firstKeptEntryId": first_kept_entry_id,
        "messagesToSummarize": preparation.messages_to_summarize,
        "turnPrefixMessages": preparation.turn_prefix_messages,
        "tokensBefore": preparation.tokens_before,
        "isSplitTurn": preparation.is_split_turn,
        "previousSummary": preparation.previous_summary,
        "fileOps": {
            "read": preparation.file_ops.read.iter().cloned().collect::<Vec<_>>(),
            "written": preparation.file_ops.written.iter().cloned().collect::<Vec<_>>(),
            "edited": preparation.file_ops.edited.iter().cloned().collect::<Vec<_>>(),
        },
        "settings": {
            "enabled": preparation.settings.enabled,
            "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        },
    });
    let branch_entries = serde_json::to_value(&entries)
        .map_err(|error| format!("{operation}: serialize branch entries: {error}"))?;
    let hook_payload = json!({
        "type": "session_before_compact",
        "preparation": preparation_value,
        "branchEntries": branch_entries,
        "customInstructions": custom_instructions,
        "reason": if force { "manual" } else { "threshold" },
        "willRetry": false,
    });
    let mut extension_result: Option<pi_agent::harness::compaction::CompactResult> = None;
    let mut extension_details = None;
    match runtime
        .extensions
        .runner
        .emit_session_before_compact(&hook_payload)
    {
        Ok(result) if result.cancelled => {
            return Err(format!("{operation}: cancelled by extension"))
        }
        Ok(result) => {
            if let Some(extension) = result.compaction {
                let kept_index = entries
                    .iter()
                    .position(|entry| entry.id() == extension.first_kept_entry_id)
                    .ok_or_else(|| {
                        format!(
                            "{operation}: extension compaction firstKeptEntryId not found: {}",
                            extension.first_kept_entry_id
                        )
                    })?;
                let retained_tail = entries[kept_index..]
                    .iter()
                    .filter_map(|entry| entry.as_message().cloned())
                    .collect::<Vec<_>>();
                extension_details = extension.details.clone();
                let details = extension.details.and_then(|value| {
                    Some(pi_agent::harness::compaction::CompactionDetails {
                        read_files: value
                            .get("readFiles")?
                            .as_array()?
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                        modified_files: value
                            .get("modifiedFiles")?
                            .as_array()?
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    })
                });
                extension_result = Some(pi_agent::harness::compaction::CompactResult {
                    summary: extension.summary,
                    tokens_before: extension.tokens_before,
                    usage: extension.usage,
                    retained_tail,
                    details,
                });
            }
        }
        Err(errors) => return Err(format!("{operation}: extension hook failed: {errors:?}")),
    }
    // Summarize through the models facade (same seam as the RPC compact).
    let models = runtime.models.clone();
    let complete_simple_fn: pi_agent::harness::CompleteSimpleFn =
        Arc::new(move |model, ctx, opts| {
            let models = models.clone();
            let opts = opts.clone();
            let model = model.clone();
            let ctx = ctx.clone();
            Box::pin(async move { models.complete_simple(&model, &ctx, Some(&opts)).await })
        });
    let options = pi_agent::harness::SimpleModels { complete_simple_fn };
    let (retry_enabled, max_retries, base_delay_ms) = settings_manager.get_retry_settings();
    let retry = pi_ai::utils::retry::RetryPolicy {
        enabled: retry_enabled,
        max_retries: u32::try_from(max_retries).unwrap_or(u32::MAX),
        base_delay_ms,
    };
    let result = match extension_result {
        Some(result) => result,
        None => pi_agent::harness::compaction::compact(
            &preparation,
            &options,
            &runtime.model,
            custom_instructions,
            None,
            None,
            Some(&retry),
            None,
        )
        .await
        .map_err(|e| format!("{operation}: {e}"))?,
    };

    // Replace the in-memory context: summary message + retained tail.
    let summary_msg = pi_agent::agent::user_text_prompt(
        format!("[Compaction summary]\n{}", result.summary),
        pi_ai::types::now_ms(),
    );
    let mut replaced = vec![summary_msg];
    replaced.extend(result.retained_tail.clone());
    runtime.messages = replaced;

    // Persist a compaction entry so the session file records the summary.
    let persisted_details = extension_details.or_else(|| {
        result.details.as_ref().map(|details| {
            json!({
                "readFiles": details.read_files,
                "modifiedFiles": details.modified_files,
            })
        })
    });
    runtime
        .session
        .append_entry(
            EntryNoStats::Compaction {
                id: format!("c-{}", pi_agent::session::new_id()),
                summary: result.summary.clone(),
                retained_tail: result.retained_tail,
                tokens_before: result.tokens_before,
                details: persisted_details,
                usage: result.usage.clone(),
            },
            "main",
        )
        .await
        .map_err(|e| format!("{operation}: persist: {e}"))?;
    // Keep a reset marker in the deferred display-entry shadow so the next
    // request cannot be mistaken for a continuation of the pre-compaction
    // prompt cache.
    runtime.cache_entries.push(json!({
        "type": "compaction",
        "timestamp": pi_ai::types::now_ms(),
        "usage": result.usage,
    }));
    runtime.persisted_until = runtime.messages.len();
    invalidate_interactive_harness(runtime);
    Ok(true)
}

/// Auto-compaction (upstream `core/compaction/` loop): after a turn, if the
/// estimated context tokens exceed the model's window minus the reserve,
/// summarize the history through the models facade and replace the in-memory
/// context with the summary plus the retained tail. Returns true when
/// compaction ran.
async fn maybe_auto_compact(
    runtime: &mut InteractiveRuntime,
    settings: &SettingsManager,
) -> Result<bool, String> {
    compact_interactive(runtime, settings, None, false).await
}

/// Short cwd for banners (home-relative like the footer).
fn meta_short_cwd(cwd: &str) -> String {
    if let Some(home) = crate::config::home_dir().map(|path| path.to_string_lossy().into_owned()) {
        if let Some(rest) = cwd.strip_prefix(&home) {
            if rest.is_empty() {
                return "~".to_string();
            }
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}

/// Aggregate cumulative usage + the latest assistant turn's cache-hit rate
/// from the in-memory transcript, for the footer token totals (upstream
/// `FooterComponent.render`).
#[cfg(test)]
fn footer_usage_from_messages(
    messages: &[pi_agent::types::AgentMessage],
) -> (Option<crate::core::usage_totals::UsageTotals>, Option<f64>) {
    use crate::core::usage_totals as ut;
    let mut totals = ut::create_usage_totals();
    let mut saw_any = false;
    let mut cache_hit_rate: Option<f64> = None;
    for message in messages {
        let assistant = match message {
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => a,
            _ => continue,
        };
        let Some(usage) = assistant.usage() else {
            continue;
        };
        saw_any = true;
        ut::add_usage_to_totals(&mut totals, usage);
        let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
        cache_hit_rate = if prompt_tokens > 0 {
            Some((usage.cache_read as f64 / prompt_tokens as f64) * 100.0)
        } else {
            None
        };
    }
    if saw_any {
        (Some(totals), cache_hit_rate)
    } else {
        (None, None)
    }
}

/// Rehydrate in-memory messages + transcript from a session's message
/// entries (oldest first), mirroring the RPC get_entries load path.
async fn rehydrate_transcript(
    runtime: &InteractiveRuntime,
    transcript_md: &Arc<Mutex<Markdown>>,
    hide_thinking: bool,
) -> (Vec<pi_agent::types::AgentMessage>, Vec<Value>) {
    let entries = runtime
        .session
        .find_entries(&pi_agent::session::state::EntryQuery {
            order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
            id: None,
            entry_type: None,
            custom_type: None,
            cursor: None,
            limit: None,
        })
        .await
        .unwrap_or_default();
    // Build the same active transcript projection that the provider sees.
    // In particular, a compaction entry becomes a visible compaction summary
    // plus its retained tail; replaying only physical message rows would
    // resurrect failed/pre-compaction history after restart.
    let messages =
        pi_agent::session::context::build_session_context(&entries, &Default::default()).messages;
    let cache_entries = entries
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    transcript_md
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .set_text(it::compose_transcript(&messages, hide_thinking, ""));
    (messages, cache_entries)
}

/// Serialize one in-memory agent message into the session-entry shape used by
/// the cache and usage analyzers. Interactive turns are persisted on exit, so
/// keeping this shadow list lets the footer and `/session` stay current.
fn cache_entry_from_message(message: &pi_agent::types::AgentMessage) -> Option<Value> {
    let timestamp = match message {
        pi_agent::types::AgentMessage::Core(Message::User(user)) => user.timestamp(),
        pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) => assistant.timestamp(),
        pi_agent::types::AgentMessage::Core(Message::ToolResult(tool)) => tool.timestamp(),
        pi_agent::types::AgentMessage::Custom(custom) => custom.timestamp(),
    };
    Some(json!({
        "type": "message",
        "timestamp": timestamp,
        "message": serde_json::to_value(message).ok()?,
    }))
}

fn append_cache_entries_from_messages(
    entries: &mut Vec<Value>,
    messages: &[pi_agent::types::AgentMessage],
) {
    entries.extend(messages.iter().filter_map(cache_entry_from_message));
}

/// Keep the composer border synchronized with the same state transitions as
/// Pi's interactive editor.  The Rust editor does not expose an `onChange`
/// callback, so the owning loop derives bash mode from the current draft and
/// applies the border before each frame.  This also makes a pasted `!` command
/// switch colors in the very next render instead of waiting for submission.
fn sync_editor_border(
    editor: &Arc<Mutex<Editor>>,
    thinking_level: &str,
    bash_mode: bool,
    last_state: &mut Option<(String, bool)>,
) {
    let state = (thinking_level.to_string(), bash_mode);
    if last_state.as_ref() == Some(&state) {
        return;
    }
    editor
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .border_color = if bash_mode {
        it::tui_theme::bash_mode_border()
    } else {
        it::tui_theme::thinking_border(thinking_level)
    };
    *last_state = Some(state);
}

/// Format one significant cache miss using the upstream labels and thresholds.
fn format_cache_miss_notice(miss: &crate::core::cache_stats::CacheMiss) -> Option<String> {
    if miss.missed_tokens < crate::core::cache_stats::CACHE_NOTICE_MIN_TOKENS
        && miss.missed_cost < crate::core::cache_stats::CACHE_NOTICE_MIN_COST
    {
        return None;
    }
    let cost = if miss.missed_cost >= 0.01 {
        format!(" (~${:.2})", miss.missed_cost)
    } else {
        String::new()
    };
    let rebilled = format!(
        "{} tokens re-billed{}",
        it::messages::format_tokens(miss.missed_tokens),
        cost
    );
    let label = if miss.model_changed {
        "Cache miss after model switch".to_string()
    } else if miss.idle_ms >= crate::core::cache_stats::CACHE_TTL_MS {
        format!(
            "Cache miss after {}m idle",
            (miss.idle_ms as f64 / 60_000.0).round() as u64
        )
    } else {
        "Cache miss".to_string()
    };
    Some(format!("⚠ {label}: {rebilled}"))
}

/// Re-derive transcript notices from the current shadow session entries. The
/// notices are keyed by the assistant entry timestamp, not vector position,
/// so compaction can replace the in-memory context without misplacing them.
fn cache_notice_timestamps(entries: &[Value]) -> Vec<(u64, String)> {
    let misses = crate::core::cache_stats::collect_cache_misses(
        entries,
        &crate::core::cache_stats::NoPrices,
    );
    misses
        .into_iter()
        .filter_map(|(index, miss)| {
            let entry = entries.get(index)?;
            if entry.get("type").and_then(Value::as_str) != Some("message")
                || entry
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    != Some("assistant")
            {
                return None;
            }
            let timestamp = entry.get("timestamp").and_then(Value::as_u64)?;
            Some((timestamp, format_cache_miss_notice(&miss)?))
        })
        .collect()
}

/// Aggregate cumulative usage from serialized entries, including summary and
/// tool-result usage that is not present in the post-compaction context.
fn footer_usage_from_entries(
    entries: &[Value],
) -> (Option<crate::core::usage_totals::UsageTotals>, Option<f64>) {
    use crate::core::usage_totals as ut;
    let mut totals = ut::create_usage_totals();
    let mut saw_any = false;
    let mut cache_hit_rate = None;
    for entry in entries {
        match ut::parse_session_entry(entry) {
            ut::SessionEntryUsageView::Assistant { usage, .. } => {
                if let Some(usage) = usage {
                    saw_any = true;
                    ut::add_usage_to_totals(&mut totals, &usage);
                    let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
                    cache_hit_rate = if prompt_tokens > 0 {
                        Some((usage.cache_read as f64 / prompt_tokens as f64) * 100.0)
                    } else {
                        None
                    };
                }
            }
            ut::SessionEntryUsageView::ToolResult { usage }
            | ut::SessionEntryUsageView::Summary { usage } => {
                saw_any = true;
                ut::add_usage_to_totals(&mut totals, &usage);
            }
            ut::SessionEntryUsageView::Other => {}
        }
    }
    if saw_any {
        (Some(totals), cache_hit_rate)
    } else {
        (None, None)
    }
}

fn format_cache_waste_line(waste: crate::core::cache_stats::CacheWasteTotals) -> Option<String> {
    if waste.missed_tokens == 0 {
        return None;
    }
    let miss_label = if waste.miss_count == 1 {
        "1 miss".to_string()
    } else {
        format!("{} misses", waste.miss_count)
    };
    let detail = format!("{} tokens, {}", waste.missed_tokens, miss_label);
    if waste.missed_cost >= 0.0001 {
        Some(format!(
            "Cache Re-billed: ${:.3} ({detail})",
            waste.missed_cost
        ))
    } else {
        Some(format!("Cache Re-billed: {detail}"))
    }
}

fn session_status(runtime: &InteractiveRuntime) -> String {
    let waste = crate::core::cache_stats::compute_cache_waste(
        &runtime.cache_entries,
        &crate::core::cache_stats::NoPrices,
    );
    let (usage, _) = footer_usage_from_entries(&runtime.cache_entries);
    let mut status = format!(
        "session {} — {} messages in transcript",
        runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
        runtime.messages.len()
    );
    if let Some(usage) = usage {
        status.push_str(&format!(
            "\nusage: {} tokens, ${:.3}",
            usage.input + usage.output + usage.cache_read + usage.cache_write,
            usage.cost
        ));
    }
    if let Some(line) = format_cache_waste_line(waste) {
        status.push('\n');
        status.push_str(&line);
    }
    status
}

/// Load the shipped changelog, allowing an explicit path to override it for
/// development/package tests. Installed binaries use the embedded catalogue.
fn changelog_content() -> String {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("PI_CHANGELOG_PATH") {
        candidates.push(std::path::PathBuf::from(path));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("CHANGELOG.md"));
    }
    candidates.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../CHANGELOG.md"));
    for path in candidates {
        if let Some(content) = crate::core::changelog::read_path(&path) {
            if !content.trim().is_empty() {
                return content;
            }
        }
    }
    crate::core::changelog::embedded_content().to_string()
}

fn changelog_status() -> String {
    format!(
        "What's New\n{}",
        crate::core::changelog::full_markdown(&changelog_content())
    )
}

/// Append in-memory messages to a session's main lane (idempotent per call).
async fn persist_entries_checked(
    session: &mut JsonlSession<pi_agent::fs::StdFileSystem>,
    entries: &[Entry],
) -> Result<(), String> {
    for entry in entries {
        session
            .append_entry(entry.to_no_stats(), "main")
            .await
            .map_err(|error| format!("persist interactive session entry: {error}"))?;
    }
    Ok(())
}

async fn persist_messages_checked(
    session: &mut JsonlSession<pi_agent::fs::StdFileSystem>,
    messages: &[pi_agent::types::AgentMessage],
) -> Result<(), String> {
    for message in messages {
        session
            .append_entry(
                EntryNoStats::Message {
                    id: format!("m-{}", pi_agent::session::new_id()),
                    message: message.clone(),
                    terminate: None,
                },
                "main",
            )
            .await
            .map_err(|error| format!("persist interactive turn: {error}"))?;
    }
    Ok(())
}

async fn persist_messages(
    session: &mut JsonlSession<pi_agent::fs::StdFileSystem>,
    messages: &[pi_agent::types::AgentMessage],
) {
    let _ = persist_messages_checked(session, messages).await;
}

/// Run the upstream `/share` flow: gh auth check -> export session HTML ->
/// `gh gist create --public=false` -> viewer URL. Returns the final status
/// message or an error. All gh calls are spawn_blocking + timeout so a
/// hanging gh never blocks the UI loop.
async fn run_gh(args: Vec<String>) -> Result<std::process::Output, String> {
    let layered = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new("gh");
            cmd.args(&args);
            cmd.output()
        }),
    )
    .await
    .map_err(|_| "gh command timed out".to_string())?;
    match layered {
        Ok(res) => res.map_err(|e| format!("gh spawn failed: {e}")),
        Err(e) => Err(format!("gh spawn failed: {e}")),
    }
}

/// Run the upstream `/share` flow: gh auth check -> export session HTML ->
/// `gh gist create --public=false` -> viewer URL. Returns the final status
/// message or an error.
fn share_viewer_url(value: Option<&str>) -> String {
    value
        .filter(|value| !value.is_empty())
        .unwrap_or("https://pi.dev/session/")
        .to_string()
}

async fn run_share(runtime: &InteractiveRuntime, dry_run: bool) -> Result<String, String> {
    if dry_run {
        return Ok("PI_SHARE_DRY_RUN=1: /share skipped".to_string());
    }
    if !runtime.session_persistence {
        return Err("/share requires a persistent session; remove --no-session".to_string());
    }
    let gh_auth = match run_gh(vec!["auth".to_string(), "status".to_string()]).await {
        Ok(out) => out,
        Err(_) => {
            return Err(
                "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
                    .to_string(),
            )
        }
    };
    if !gh_auth.status.success() {
        return Err("GitHub CLI is not logged in. Run 'gh auth login' first.".to_string());
    }
    let meta = runtime.session.get_metadata().await;
    let tmp_file = std::env::temp_dir().join(format!("pi-share-{}.html", std::process::id()));
    let tmp_path = tmp_file.to_string_lossy().into_owned();
    crate::core::export_html::export_session_file(&meta.path, Some(&tmp_path), None)
        .map_err(|e| format!("failed to export session: {e}"))?;
    let gh_gist = run_gh(vec![
        "gist".to_string(),
        "create".to_string(),
        "--public=false".to_string(),
        tmp_path.clone(),
    ])
    .await?;
    let _ = std::fs::remove_file(&tmp_path);
    if !gh_gist.status.success() {
        return Err(format!(
            "failed to create gist: {}",
            String::from_utf8_lossy(&gh_gist.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&gh_gist.stdout);
    let gist_url = stdout.lines().next().unwrap_or("").trim().to_string();
    let gist_id = gist_url.rsplit('/').next().unwrap_or("").to_string();
    let viewer = share_viewer_url(std::env::var("PI_SHARE_VIEWER_URL").ok().as_deref());
    Ok(format!("Share URL: {viewer}#{gist_id}\nGist: {gist_url}"))
}

/// TUI-backed auth interaction (upstream `AuthInteraction`): notifications go
/// to the status banner and prompts are rendered/read through the active raw
/// terminal, so OAuth callbacks can cancel an in-flight prompt safely. The
/// shared auth surface is the editor-slot modal projection; provider/network
/// behavior remains in `pi-ai`.
struct TuiAuthInteraction {
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
    surface: Arc<Mutex<AuthSurfaceState>>,
}

impl TuiAuthInteraction {
    fn new(
        banner: Arc<Mutex<String>>,
        terminal: Arc<Mutex<TerminalBackend>>,
        surface: Arc<Mutex<AuthSurfaceState>>,
    ) -> Self {
        Self {
            banner,
            terminal,
            surface,
        }
    }

    fn begin_dialog(&self, title: impl Into<String>) {
        self.surface
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_dialog(title);
        render_auth_surface(&self.surface, &self.terminal);
    }
}

fn open_auth_browser(url: &str) -> bool {
    if std::env::var_os("PI_OAUTH_NO_BROWSER").is_some() {
        return false;
    }
    for command in ["xdg-open", "gio"] {
        let mut process = std::process::Command::new(command);
        if command == "gio" {
            process.arg("open");
        }
        if process
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Render an auth component in place without clearing the transcript. The
/// next normal scene render invalidates the retained frame and restores the
/// editor after the provider flow completes.
fn render_auth_surface(
    surface: &Arc<Mutex<AuthSurfaceState>>,
    terminal: &Arc<Mutex<TerminalBackend>>,
) {
    let width = terminal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .width()
        .max(24);
    let (lines, previous_lines) = {
        let mut state = surface.lock().unwrap_or_else(|error| error.into_inner());
        let lines = state.render_lines(width);
        let previous_lines = state.rendered_lines();
        state.set_rendered_lines(lines.len());
        (lines, previous_lines)
    };

    let mut output = String::new();
    if previous_lines > 0 {
        output.push_str(&format!("\x1b[{previous_lines}A\r"));
    } else {
        output.push('\r');
    }
    let rows = previous_lines.max(lines.len());
    for index in 0..rows {
        if index > 0 {
            output.push_str("\r\n");
        }
        output.push_str("\x1b[2K\r");
        if let Some(line) = lines.get(index) {
            output.push_str(line);
        }
    }
    output.push_str("\r\n");
    terminal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .write_raw(&output);
}

/// Read a short auth answer through the terminal backend while retaining raw
/// mode. `next_event` has a bounded poll timeout, so an OAuth callback can set
/// `abort` and wake this prompt without leaving a blocked stdin reader behind.
fn prompt_terminal_with_abort(
    surface: &Arc<Mutex<AuthSurfaceState>>,
    terminal: &Arc<Mutex<TerminalBackend>>,
    prompt: &pi_ai::auth::AuthPrompt,
    abort: &AtomicBool,
) -> Result<String, String> {
    {
        let mut state = surface.lock().unwrap_or_else(|error| error.into_inner());
        match prompt {
            pi_ai::auth::AuthPrompt::Select { message, options } => state.set_selector(
                message.clone(),
                options.clone(),
                message.to_ascii_lowercase().contains("provider"),
            ),
            _ => state.set_prompt(prompt),
        }
    }
    render_auth_surface(surface, terminal);
    let result = loop {
        if abort.load(Ordering::SeqCst) {
            break Err("Login cancelled".to_string());
        }
        let event = match terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .next_event()
        {
            Ok(event) => event,
            Err(error) => break Err(format!("read auth input: {error}")),
        };
        match event {
            pi_tui::terminal::TerminalEvent::Resize(_, _) => {
                render_auth_surface(surface, terminal);
                continue;
            }
            pi_tui::terminal::TerminalEvent::Key(raw) => {
                if raw.is_empty() || is_key_release(&raw) {
                    continue;
                }
                let action = surface
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .handle_raw(&raw);
                match action {
                    AuthSurfaceAction::Submit(value) => break Ok(value),
                    AuthSurfaceAction::Cancel => {
                        abort.store(true, Ordering::SeqCst);
                        break Err("Login cancelled".to_string());
                    }
                    AuthSurfaceAction::None => render_auth_surface(surface, terminal),
                }
            }
        }
    };
    result
}

impl pi_ai::auth::AuthInteraction for TuiAuthInteraction {
    fn supports_async_prompt(&self) -> bool {
        true
    }

    fn prompt(&self, prompt: &pi_ai::auth::AuthPrompt) -> Result<String, String> {
        let abort = AtomicBool::new(false);
        let answer = prompt_terminal_with_abort(&self.surface, &self.terminal, prompt, &abort)?;
        let answer = answer.trim();
        if let pi_ai::auth::AuthPrompt::Select { options, .. } = prompt {
            // Selectors return canonical ids. Keep accepting a typed numeric
            // index for existing scripted PTY flows without displaying row
            // numbers in the Pi-style component.
            if answer.is_empty() {
                if let Some(option) = options.first() {
                    return Ok(option.id.clone());
                }
            }
            if let Ok(index) = answer.parse::<usize>() {
                if let Some(option) = options.get(index.saturating_sub(1)) {
                    return Ok(option.id.clone());
                }
            }
            if options.iter().any(|option| option.id == answer) {
                return Ok(answer.to_string());
            }
        }
        Ok(answer.to_string())
    }

    fn prompt_async_with_abort<'a>(
        &'a self,
        prompt: &'a pi_ai::auth::AuthPrompt,
        abort: Arc<AtomicBool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        let terminal = self.terminal.clone();
        let surface = self.surface.clone();
        let prompt = prompt.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                prompt_terminal_with_abort(&surface, &terminal, &prompt, &abort)
            })
            .await
            .map_err(|error| format!("auth prompt task failed: {error}"))?
        })
    }

    fn notify(&self, event: &pi_ai::auth::AuthEvent) {
        let msg = match event {
            pi_ai::auth::AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                let browser = open_auth_browser(verification_uri);
                let prefix = if browser {
                    "A browser window should open."
                } else {
                    "Open this URL in a browser."
                };
                self.surface
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .show_device_code(verification_uri, user_code);
                format!("{prefix} {verification_uri} and enter code: {user_code}")
            }
            pi_ai::auth::AuthEvent::AuthUrl { url, instructions } => {
                let browser = open_auth_browser(url);
                let prefix = if browser {
                    "A browser window should open."
                } else {
                    "Open this URL to sign in:"
                };
                self.surface
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .show_auth(url, instructions.as_deref());
                format!("{prefix} {url}")
            }
            pi_ai::auth::AuthEvent::Progress { message } => {
                self.surface
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .show_progress(message);
                message.clone()
            }
            pi_ai::auth::AuthEvent::Info { message, links } => {
                self.surface
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .show_info(message, links);
                message.clone()
            }
        };
        *self
            .banner
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = msg;
        render_auth_surface(&self.surface, &self.terminal);
    }
}

fn auth_selector_status(
    models: &pi_ai::models::Models,
    provider_id: &str,
    auth_type: &str,
) -> String {
    let Some(status) = models.check_auth(provider_id) else {
        return "unconfigured".to_string();
    };
    if status.auth_type != auth_type {
        return if status.auth_type == "oauth" {
            "subscription configured".to_string()
        } else {
            "API key configured".to_string()
        };
    }
    match status.source.as_deref() {
        None | Some("OAuth") | Some("stored credential") => "configured".to_string(),
        Some(source)
            if source.chars().all(|character| {
                character.is_ascii_uppercase()
                    || character.is_ascii_digit()
                    || character == '_'
                    || character == ','
                    || character == ' '
            }) =>
        {
            format!("env: {source}")
        }
        Some(source) => source.to_string(),
    }
}

/// Run the upstream `/login <provider>` OAuth flow: find the provider in the
/// models registry, run its OAuth login, store the credential. Returns the
/// final status message or an error.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
async fn run_oauth_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
    surface: Arc<Mutex<AuthSurfaceState>>,
) -> Result<String, String> {
    let mut providers: Vec<pi_ai::models::Provider> = models
        .get_providers()
        .into_iter()
        .filter(|p| p.auth.oauth.is_some())
        .collect();
    providers.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    if providers.is_empty() {
        return Err(match provider_ref {
            Some(r) => format!("no OAuth login available for provider {r:?}"),
            None => "no OAuth-capable providers registered".to_string(),
        });
    }
    let interaction = TuiAuthInteraction::new(banner, terminal, surface);
    let selected_provider = match provider_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(provider) => provider.to_string(),
        None => interaction.prompt(&pi_ai::auth::AuthPrompt::Select {
            message: "Select provider to configure:".to_string(),
            options: providers
                .iter()
                .map(|provider| pi_ai::auth::AuthSelectOption {
                    id: provider.id.clone(),
                    label: provider.name.clone(),
                    description: Some(auth_selector_status(models, &provider.id, "oauth")),
                })
                .collect(),
        })?,
    };
    let provider = providers
        .iter()
        .find(|provider| {
            provider.id.eq_ignore_ascii_case(&selected_provider)
                || provider.name.eq_ignore_ascii_case(&selected_provider)
        })
        .ok_or_else(|| format!("no OAuth login available for provider {selected_provider:?}"))?;
    let oauth = provider.auth.oauth.as_ref().expect("filtered for oauth");
    interaction.begin_dialog(format!("Login to {}", provider.name));
    let credential = oauth.login(&interaction).await.map_err(|e| e.to_string())?;
    let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
    let opts = crate::core::auth_storage::AuthOperationOptions::default();
    let cred = crate::core::auth_storage::Credential::OAuth {
        access: credential.access,
        refresh: credential.refresh,
        expires: credential.expires,
        extra: credential.extra,
    };
    let provider_id = provider.id.clone();
    auth.modify(
        &provider_id,
        move |_| {
            let cred = cred.clone();
            Box::pin(async move { Ok(Some(cred)) })
        },
        &opts,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(format!("logged in to {provider_id} via OAuth"))
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
async fn run_api_key_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
    surface: Arc<Mutex<AuthSurfaceState>>,
) -> Result<String, String> {
    if provider_ref.is_some_and(|provider| {
        provider
            .trim()
            .eq_ignore_ascii_case(crate::core::llama::LLAMA_PROVIDER_ID)
    }) {
        // llama.cpp is a hidden/native provider in the upstream extension and
        // is registered lazily.  An explicit `/login llama.cpp` must still
        // reach its real URL/key prompt when the active model is OpenAI.
        crate::interactive::llama::register_provider(models);
    }
    let mut providers: Vec<pi_ai::models::Provider> = models
        .get_providers()
        .into_iter()
        .filter(|provider| provider.auth.api_key.is_some())
        .collect();
    providers.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    if providers.is_empty() {
        return Err("no API-key providers registered".to_string());
    }
    let interaction = TuiAuthInteraction::new(banner, terminal, surface);
    let selected_provider = match provider_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(provider) => provider.to_string(),
        None => interaction.prompt(&pi_ai::auth::AuthPrompt::Select {
            message: "Select provider to configure with an API key:".to_string(),
            options: providers
                .iter()
                .map(|provider| pi_ai::auth::AuthSelectOption {
                    id: provider.id.clone(),
                    label: provider.name.clone(),
                    description: Some(auth_selector_status(models, &provider.id, "api_key")),
                })
                .collect(),
        })?,
    };
    let provider = providers
        .iter()
        .find(|provider| {
            provider.id.eq_ignore_ascii_case(&selected_provider)
                || provider.name.eq_ignore_ascii_case(&selected_provider)
        })
        .ok_or_else(|| format!("no API-key login available for provider {selected_provider:?}"))?;
    let api_key_auth = provider
        .auth
        .api_key
        .as_ref()
        .expect("filtered for api_key");
    interaction.begin_dialog(format!("Login to {}", provider.name));
    let credential = pi_ai::auth::ApiKeyAuth::login(api_key_auth.as_ref(), &interaction)
        .map_err(|e| e.to_string())?;
    let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
    let opts = crate::core::auth_storage::AuthOperationOptions::default();
    let provider_id = provider.id.clone();
    auth.modify(
        &provider_id,
        move |_| {
            let credential = credential.clone();
            Box::pin(async move {
                Ok(Some(crate::core::auth_storage::Credential::ApiKey {
                    key: credential.key,
                    env: credential.env,
                }))
            })
        },
        &opts,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(format!("logged in to {provider_id} via API key"))
}

async fn run_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
    surface: Arc<Mutex<AuthSurfaceState>>,
) -> Result<String, String> {
    if provider_ref.is_some_and(|provider| {
        provider
            .trim()
            .eq_ignore_ascii_case(crate::core::llama::LLAMA_PROVIDER_ID)
    }) {
        crate::interactive::llama::register_provider(models);
    }
    let providers = models.get_providers();
    let interaction = TuiAuthInteraction::new(banner.clone(), terminal.clone(), surface.clone());
    let method = if let Some(provider_ref) = provider_ref {
        let provider = providers
            .iter()
            .find(|provider| {
                provider.id.eq_ignore_ascii_case(provider_ref.trim())
                    || provider.name.eq_ignore_ascii_case(provider_ref.trim())
            })
            .ok_or_else(|| format!("no login method available for provider {provider_ref:?}"))?;
        match (
            provider.auth.oauth.is_some(),
            provider.auth.api_key.is_some(),
        ) {
            (true, true) => interaction.prompt(&pi_ai::auth::AuthPrompt::Select {
                message: format!("Select authentication method for {}:", provider.name),
                options: vec![
                    pi_ai::auth::AuthSelectOption {
                        id: "oauth".to_string(),
                        label: provider
                            .auth
                            .oauth
                            .as_ref()
                            .and_then(|oauth| oauth.login_label().map(str::to_string))
                            .unwrap_or_else(|| "Sign in with an account".to_string()),
                        description: None,
                    },
                    pi_ai::auth::AuthSelectOption {
                        id: "api_key".to_string(),
                        label: "Sign in with an API key".to_string(),
                        description: None,
                    },
                ],
            })?,
            (true, false) => "oauth".to_string(),
            (false, true) => "api_key".to_string(),
            (false, false) => return Err(format!("provider {provider_ref:?} has no login method")),
        }
    } else {
        let has_oauth = providers
            .iter()
            .any(|provider| provider.auth.oauth.is_some());
        let has_api_key = providers
            .iter()
            .any(|provider| provider.auth.api_key.is_some());
        match (has_oauth, has_api_key) {
            (true, true) => interaction.prompt(&pi_ai::auth::AuthPrompt::Select {
                message: "Select authentication method:".to_string(),
                options: vec![
                    pi_ai::auth::AuthSelectOption {
                        id: "oauth".to_string(),
                        label: "Sign in with an account".to_string(),
                        description: Some("subscription providers".to_string()),
                    },
                    pi_ai::auth::AuthSelectOption {
                        id: "api_key".to_string(),
                        label: "Sign in with an API key".to_string(),
                        description: Some("API providers".to_string()),
                    },
                ],
            })?,
            (true, false) => "oauth".to_string(),
            (false, true) => "api_key".to_string(),
            (false, false) => return Err("no login providers registered".to_string()),
        }
    };
    match method.as_str() {
        "oauth" => run_oauth_login(models, provider_ref, banner, terminal, surface).await,
        "api_key" => run_api_key_login(models, provider_ref, banner, terminal, surface).await,
        other => Err(format!("unknown authentication method {other:?}")),
    }
}

/// Remove one credential saved by `/login`. With no provider argument this
/// mirrors Pi's selector instead of making the user guess the stored id.
async fn run_oauth_logout(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
    surface: Arc<Mutex<AuthSurfaceState>>,
) -> Result<String, String> {
    let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
    let opts = crate::core::auth_storage::AuthOperationOptions::default();
    let credentials = auth.list(&opts).await.map_err(|e| e.to_string())?;
    if credentials.is_empty() && provider_ref.is_none() {
        return Ok("No stored credentials to remove. Environment variables and models.json config are unchanged.".to_string());
    }

    if let Some(provider) = provider_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        auth.delete(provider, &opts)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(format!("logged out {provider}"));
    }

    let interaction = TuiAuthInteraction::new(banner, terminal, surface);
    let selected_provider = interaction.prompt(&pi_ai::auth::AuthPrompt::Select {
        message: "Select provider to logout:".to_string(),
        options: credentials
            .iter()
            .map(|credential| pi_ai::auth::AuthSelectOption {
                id: credential.provider_id.clone(),
                label: models
                    .get_provider(&credential.provider_id)
                    .map(|provider| provider.name)
                    .unwrap_or_else(|| credential.provider_id.clone()),
                description: Some(credential.credential_type.to_string()),
            })
            .collect(),
    })?;
    let provider_id = credentials
        .iter()
        .find(|credential| {
            credential.provider_id == selected_provider
                || models
                    .get_provider(&credential.provider_id)
                    .is_some_and(|provider| provider.name == selected_provider)
        })
        .map(|credential| credential.provider_id.clone())
        .ok_or_else(|| format!("no stored credentials for provider {selected_provider:?}"))?;
    auth.delete(&provider_id, &opts)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("logged out {provider_id}"))
}

/// Wrap a modal in a renderable SharedComponent for the frame.
fn modal_shared(modal: &mut Modal) -> SharedComponent {
    match modal {
        Modal::Model(sel) => sel.clone() as SharedComponent,
        Modal::Thinking(sel) => sel.clone() as SharedComponent,
        Modal::Theme(sel) | Modal::Fork(sel) => sel.clone() as SharedComponent,
        Modal::Llama(sel) => sel.clone() as SharedComponent,
        Modal::LlamaLoadPlan { selector, .. } => selector.clone() as SharedComponent,
        Modal::LlamaUnloadConfirm { selector, .. } => selector.clone() as SharedComponent,
        Modal::HuggingFace(sel) => sel.clone() as SharedComponent,
        Modal::HuggingFaceDownload(sel) => sel.clone() as SharedComponent,
        Modal::ScopedModels(sel) => sel.clone() as SharedComponent,
        Modal::Settings(panel) => panel.clone() as SharedComponent,
        Modal::Resume(sel, _) => sel.clone() as SharedComponent,
        Modal::CrossProjectSession(prompt) => prompt.clone() as SharedComponent,
        Modal::Trust(sel) => sel.clone() as SharedComponent,
        Modal::Tree(sel) => sel.clone() as SharedComponent,
    }
}

fn missing_session_id_warning(session_id: &str) -> String {
    format!(
        "Warning: No project session found with id '{session_id}'; creating a new session with that id."
    )
}

/// The interactive main loop. Returns Ok(()) on clean exit.
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
pub async fn run_interactive_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut settings = settings;
    let cwd = config::cwd();
    let mut provider = crate::run::resolve_run_provider(
        args.provider.as_deref(),
        args.model.as_deref(),
        &settings,
    );
    let base_models = crate::core::model_registry::builtin_models();
    let faux_core = if provider == "faux" {
        Some(crate::core::model_runtime::register_faux_provider(
            &base_models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        ))
    } else {
        None
    };
    let model_registry = crate::core::model_registry::ModelRegistry::new(
        base_models,
        crate::core::model_config::ModelConfig::load(
            crate::core::model_config::models_json_path().as_deref(),
        ),
    );
    let models = model_registry.into_models();
    provider = crate::run::canonicalize_registered_provider(&models, &provider);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
        Some(&provider),
    );
    crate::core::model_runtime::register_llama_provider_if_selected(
        &models,
        &provider,
        !args.offline && !config::env_flag(config::ENV_OFFLINE),
    )
    .await?;

    // Session repo + initial session. Keep the repository object available to
    // the mode even for an ephemeral run, but do not create its directory or
    // resolve persistent selectors when `--no-session` is active.
    let session_root = crate::run::resolve_session_root(args, Some(&settings));
    let session_persistence = !args.no_session;
    let selects_existing =
        args.continue_session || args.resume || args.session.is_some() || args.fork.is_some();
    if !session_persistence && selects_existing {
        return Err(
            "--continue, --resume, --session, and --fork require session persistence".to_string(),
        );
    }
    if session_persistence {
        std::fs::create_dir_all(&session_root).map_err(|e| format!("create session dir: {e}"))?;
        crate::core::session_migration::migrate_legacy_sessions_in_root(std::path::Path::new(
            &session_root,
        ))
        .map_err(|e| format!("migrate legacy sessions: {e}"))?;
    }
    let mut repo = JsonlSessionRepo::new(pi_agent::fs::StdFileSystem::new(&cwd), &session_root);
    let mut initial_status_banner = String::new();
    let source_selector = args.fork.as_deref().or(args.session.as_deref());
    let mut cross_project_source = None;
    // `--resume` is an interactive picker in Pi.  Keep `--continue` as the
    // explicit newest-session shortcut, while bootstrapping the resume picker
    // with an in-memory session so cancelling never creates a new JSONL file.
    let startup_resume_picker = args.resume && source_selector.is_none();
    let mut session = if !session_persistence {
        let mut metadata = in_memory_metadata(
            args.session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
                .unwrap_or_else(|| format!("interactive-{}", pi_agent::session::new_id())),
            None,
        );
        metadata.cwd = cwd.clone();
        let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(metadata)));
        JsonlSession::from_in_memory(storage)
    } else if let Some(selector) = source_selector {
        let selected_path = crate::run::resolve_session_selector_path(selector, &cwd);
        if selected_path.is_file() {
            crate::core::session_migration::migrate_legacy_session_file(&selected_path)
                .map_err(|e| format!("migrate selected session: {e}"))?;
        }
        let source = crate::run::resolve_session_metadata(&repo, selector, &cwd).await?;
        if args.fork.is_some() {
            if let Some(session_id) = args.session_id.as_deref() {
                if crate::run::find_local_session_by_id(&repo, &cwd, session_id)
                    .await?
                    .is_some()
                {
                    return Err(format!("Session already exists with id '{session_id}'"));
                }
            }
            let new_id = args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
                .unwrap_or_else(pi_agent::session::new_id);
            let session = repo
                .fork(
                    &source,
                    CreateOptions {
                        id: Some(new_id.clone()),
                        cwd: cwd.clone(),
                        parent_session_id: None,
                        metadata: None,
                        fork_options: ForkOptions::Tree,
                    },
                )
                .await
                .map_err(|e| format!("fork session {}: {e}", source.id))?;
            initial_status_banner = format!(
                "forked session {} into {}",
                source.id.get(..8).unwrap_or(&source.id),
                new_id.get(..8).unwrap_or(&new_id)
            );
            session
        } else if !it::session_meta::session_cwds_match(&source.cwd, &cwd) {
            // Pi never silently opens a session from another project. Defer
            // the real fork until the native TUI confirmation is answered so
            // cancellation cannot create a durable file or alter the source.
            cross_project_source = Some(source.clone());
            let mut metadata = in_memory_metadata(
                format!("interactive-session-{}", pi_agent::session::new_id()),
                None,
            );
            metadata.cwd = cwd.clone();
            let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(metadata)));
            JsonlSession::from_in_memory(storage)
        } else {
            let session = repo
                .open(&source)
                .await
                .map_err(|e| format!("open session {}: {e}", source.id))?;
            initial_status_banner = format!(
                "resumed session {}",
                source.id.get(..8).unwrap_or(&source.id)
            );
            session
        }
    } else if startup_resume_picker {
        let mut metadata = in_memory_metadata(
            format!("interactive-resume-{}", pi_agent::session::new_id()),
            None,
        );
        metadata.cwd = cwd.clone();
        let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(metadata)));
        JsonlSession::from_in_memory(storage)
    } else if args.continue_session {
        let mut sessions = repo
            .list(Some(&cwd))
            .await
            .map_err(|e| format!("list sessions: {e}"))?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_at));
        let source = sessions
            .into_iter()
            .next()
            .ok_or_else(|| "no previous session found to continue in this directory".to_string())?;
        let session = repo
            .open(&source)
            .await
            .map_err(|e| format!("open session {}: {e}", source.id))?;
        initial_status_banner = format!(
            "continued session {}",
            source.id.get(..8).unwrap_or(&source.id)
        );
        session
    } else if let Some(session_id) = args.session_id.as_deref() {
        if let Some(source) = crate::run::find_local_session_by_id(&repo, &cwd, session_id).await? {
            let session = repo
                .open(&source)
                .await
                .map_err(|e| format!("open session {}: {e}", source.id))?;
            initial_status_banner = format!(
                "resumed session {}",
                source.id.get(..8).unwrap_or(&source.id)
            );
            session
        } else {
            eprintln!("{}", missing_session_id_warning(session_id));
            repo.create(CreateOptions {
                id: Some(session_id.to_string()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .map_err(|e| format!("create session: {e}"))?
        }
    } else {
        repo.create(CreateOptions {
            id: args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
            cwd: cwd.clone(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Tree,
        })
        .await
        .map_err(|e| format!("create session: {e}"))?
    };
    if let Some(name) = &args.name {
        let normalized_name = crate::run::normalize_session_name_value(name);
        if normalized_name.is_empty() {
            return Err("--name requires a non-empty value".to_string());
        }
        session
            .set_name(Some(&normalized_name))
            .await
            .map_err(|e| format!("set session name: {e}"))?;
    }
    let session_id = session.get_metadata().await.id;
    let session_name = session.get_name().await;
    let configured_thinking_level = args.thinking.clone().unwrap_or_else(|| {
        settings
            .get_default_thinking_level()
            .map(str::to_string)
            .unwrap_or_else(|| crate::core::model_resolver::DEFAULT_THINKING_LEVEL.to_string())
    });
    let agent_dir = config::get_agent_dir().to_string_lossy().into_owned();
    let extensions = load_for_mode(
        args,
        &settings,
        &cwd,
        &agent_dir,
        "interactive",
        true,
        session_name.clone(),
        configured_thinking_level.clone(),
    );
    for error in &extensions.errors {
        tracing::warn!(path = %error.path, error = %error.error, "failed to load extension");
    }

    register_loaded_native_providers(&models, &extensions)
        .map_err(|error| format!("failed to register interactive native providers: {error}"))?;

    let (scoped_models, scope_diagnostics) =
        crate::run::resolve_effective_model_scope(args, &settings, &models.get_models(None));
    for diagnostic in scope_diagnostics {
        eprintln!("Warning: {}", diagnostic.message);
    }
    if let Some(message) =
        model_scope_startup_message(&scoped_models, args.verbose, settings.get_quiet_startup())
    {
        println!("{}", it::tui_theme::fg("dim", message));
    }
    let has_explicit_model = args.model.as_deref().is_some_and(|model| !model.is_empty());
    let initial_scoped_model = if !has_explicit_model && !selects_existing {
        scoped_models
            .first()
            .map(|scoped| (scoped.model.clone(), scoped.thinking_level.clone()))
    } else {
        None
    };
    if let Some((model, _)) = &initial_scoped_model {
        provider = model.provider.clone();
    }
    let scoped_thinking_level = initial_scoped_model
        .as_ref()
        .and_then(|(_, thinking_level)| thinking_level.clone());
    let scoped_model_references = scoped_models
        .iter()
        .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
        .collect::<Vec<_>>();
    let model = if let Some((model, _)) = initial_scoped_model {
        model
    } else if provider == "faux" {
        let core = faux_core.as_ref().expect("faux core registered");
        match model_hint.as_deref() {
            Some(hint) => {
                let resolved = crate::core::model_resolver::resolve_cli_model(
                    args.provider.as_deref(),
                    Some(hint),
                    args.thinking.as_deref(),
                    &core.models,
                );
                if let Some(warning) = resolved.warning {
                    initial_status_banner.push_str(&format!("Warning: {warning}\n"));
                }
                if let Some(error) = resolved.error {
                    return Err(error);
                }
                resolved
                    .model
                    .ok_or_else(|| format!("unknown faux model {hint:?}"))?
            }
            None => core
                .models
                .first()
                .cloned()
                .ok_or_else(|| "no faux model".to_string())?,
        }
    } else {
        crate::run::require_authenticated_implicit_model(
            &models,
            &provider,
            model_hint.as_deref(),
        )?;
        crate::core::model_runtime::resolve_run_model_for_provider(
            &models,
            &provider,
            model_hint.as_deref(),
        )?
    };
    let initial_thinking_level = if args.thinking.is_none() {
        scoped_thinking_level.unwrap_or_else(|| {
            settings
                .get_model_thinking_level(&provider, &model.id)
                .map(str::to_string)
                .unwrap_or(configured_thinking_level.clone())
        })
    } else {
        configured_thinking_level
    };
    let session_path = session.get_metadata().await.path;
    let _session_environment = crate::core::session_env::install(
        &session_id,
        &session_path,
        &provider,
        &model.name,
        &initial_thinking_level,
    );

    let extension_resources = extensions.resources.clone();
    let skills = load_interactive_skills(args, &cwd, &agent_dir, &settings, &extension_resources);
    register_interactive_themes(args, &settings, &extension_resources, &cwd);
    let system_prompt =
        interactive_system_prompt(args, &cwd, &agent_dir, &settings, &extension_resources);
    let prompt_templates = crate::run::load_prompt_templates_for_run(
        args,
        &cwd,
        std::path::Path::new(&agent_dir),
        &extension_resources,
    );
    let (provider_timeout_ms, provider_max_retries, max_retry_delay_ms) =
        settings.get_provider_retry_settings();
    let mut runtime = InteractiveRuntime {
        cwd: cwd.clone(),
        model_registry,
        models,
        faux_core,
        provider: provider.clone(),
        model: model.clone(),
        scoped_models: scoped_model_references,
        messages: Vec::new(),
        session,
        repo,
        session_root: session_root.clone(),
        session_id: session_id.clone(),
        session_name,
        session_persistence,
        system_prompt,
        tools_enabled: crate::run::should_register_extension_tools(args),
        builtin_tools_enabled: crate::run::should_register_builtin_tools(args),
        default_tool_names: settings.get_default_tools(),
        native_provider_ids: loaded_native_provider_ids(&extensions),
        extensions,
        extension_resources,
        skills,
        prompt_templates,
        extension_args: args.clone(),
        extension_agent_dir: agent_dir.clone(),
        auto_resize_images: settings.get_image_auto_resize(),
        block_images: settings.get_block_images(),
        shell_command_prefix: settings.get_shell_command_prefix().map(str::to_string),
        shell_path: settings.get_shell_path(),
        transport: settings.get_transport().to_string(),
        http_idle_timeout_ms: settings.get_http_idle_timeout_ms().unwrap_or(300_000),
        provider_timeout_ms,
        provider_max_retries: provider_max_retries
            .map(|retries| u32::try_from(retries).unwrap_or(u32::MAX)),
        max_retry_delay_ms,
        websocket_connect_timeout_ms: settings.get_websocket_connect_timeout_ms().ok().flatten(),
        retry_policy: crate::run::retry_policy_from_settings(&settings),
        compaction_settings: interactive_compaction_settings(&settings),
        persisted_until: 0,
        active_tool_names: None,
        cache_entries: Vec::new(),
        interactive_harness: None,
        interactive_event_handler: None,
        interactive_tool_event_handler: None,
        extensions_shutdown: false,
    };

    // Match upstream startup changelog behavior: resumed sessions do not get
    // release notes, a first install records the current version silently,
    // and a version change displays only the newly released entries.
    let mut startup_changelog = None;
    let mut should_report_install_telemetry = false;
    if initial_status_banner.is_empty() && !startup_resume_picker {
        let content = changelog_content();
        let last_version = settings.get_last_changelog_version().map(str::to_string);
        match last_version {
            None => {
                settings.set_last_changelog_version(config::VERSION.to_string());
                should_report_install_telemetry = true;
            }
            Some(last_version) => {
                if let Some(markdown) =
                    crate::core::changelog::new_markdown(&content, &last_version)
                {
                    let latest_version = crate::core::changelog::parse_changelog(&content)
                        .first()
                        .map(|entry| entry.version())
                        .unwrap_or_else(|| config::VERSION.to_string());
                    startup_changelog = Some(if settings.get_collapse_changelog() {
                        format!(
                            "Updated to v{latest_version}. Use /changelog to view full changelog."
                        )
                    } else {
                        format!("What's New\n{markdown}")
                    });
                    settings.set_last_changelog_version(config::VERSION.to_string());
                    should_report_install_telemetry = true;
                }
            }
        }
    }
    if should_report_install_telemetry {
        let telemetry_enabled =
            crate::core::telemetry::is_install_telemetry_enabled_from_env(&settings);
        if telemetry_enabled
            && !crate::core::telemetry::is_offline_env_active(
                std::env::var("PI_OFFLINE").ok().as_deref(),
            )
        {
            tokio::spawn(crate::core::telemetry::report_install_telemetry(
                config::VERSION,
                telemetry_enabled,
            ));
        }
    }

    // Load the startup picker inventory before entering raw mode.  A broken
    // session directory therefore reports as an ordinary startup error and
    // cannot strand the terminal in raw mode.  The selector itself remains
    // the same real ListSelector used by `/resume`, including fuzzy id/path
    // search and the normal cancel key handling.
    let startup_resume_sessions = if startup_resume_picker {
        Some(
            runtime
                .repo
                // Upstream starts the picker in the current-folder scope but
                // loads the all-project inventory as well so Tab can switch
                // scope without reopening the modal.
                .list(None)
                .await
                .map_err(|e| format!("list sessions: {e}"))?,
        )
    } else {
        None
    };

    // Terminal + components.
    let terminal = Arc::new(Mutex::new(TerminalBackend::new()));
    let tui_mode = args
        .tui_mode
        .as_deref()
        .unwrap_or_else(|| settings.get_tui_mode());
    let mut use_alt_screen = tui_mode == "fullscreen";
    let mut shutdown_signals = InteractiveShutdownSignals::new()?;
    #[cfg(unix)]
    let mut sigcont = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
        unix_suspend::SIGCONT,
    ))
    .map_err(|error| format!("watch SIGCONT: {error}"))?;
    terminal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .enter_raw_with_alt_screen(use_alt_screen)
        .map_err(|e| format!("enter raw: {e}"))?;
    let _terminal_guard = InteractiveTerminalGuard {
        terminal: terminal.clone(),
    };

    load_interactive_theme_setting(
        settings
            .get_theme_setting()
            .unwrap_or(crate::theme::DEFAULT_THEME),
    );
    let mut hide_thinking = initial_thinking_level == "off" || settings.get_hide_thinking_block();
    let mut thinking_level = initial_thinking_level;

    let mut editor = it::create_editor_with_skills(
        cwd.clone(),
        &runtime.skills,
        settings.get_enable_skill_commands(),
    );
    editor.set_terminal_rows(
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .height(),
    );
    editor.set_padding_x(settings.get_editor_padding_x() as usize);
    let editor: Arc<Mutex<Editor>> = Arc::new(Mutex::new(editor));

    // Keep the renderer mode behind a shared value so `/reload` can change
    // Mermaid behavior without rebuilding the transcript component. The
    // transform only substitutes diagrams that the Rust renderer can prove it
    // understands; unsupported diagrams stay as source with a warning.
    let mermaid_mode = Arc::new(Mutex::new(
        settings.get_mermaid_rendering_mode().to_string(),
    ));
    let mermaid_streaming = Arc::new(AtomicBool::new(false));
    let mermaid_mode_for_transform = Arc::clone(&mermaid_mode);
    let mermaid_streaming_for_transform = Arc::clone(&mermaid_streaming);
    let markdown_options = pi_tui::components::markdown::MarkdownOptions {
        transform: Some(Box::new(move |markdown, width| {
            let mode = mermaid_mode_for_transform
                .lock()
                .map(|mode| mode.clone())
                .unwrap_or_else(|_| "off".to_string());
            it::mermaid::transform_markdown_with_context(
                markdown,
                width,
                &mode,
                mermaid_streaming_for_transform.load(Ordering::Acquire),
                "assistant",
            )
        })),
        ..Default::default()
    };
    let transcript_md: Arc<Mutex<Markdown>> = Arc::new(Mutex::new(Markdown::new(
        String::new(),
        1,
        0,
        it::tui_theme::markdown_theme(),
        None,
        Some(markdown_options),
    )));

    // CLI startup selectors (`--continue`, `--resume`, `--session`, and
    // `--fork`) open the target before the TUI starts. Rehydrate the visible
    // transcript and cache shadow now so the first rendered frame and the
    // first prompt observe the same history as slash-command resume/import.
    if !initial_status_banner.is_empty() {
        let (messages, cache_entries) =
            rehydrate_transcript(&runtime, &transcript_md, hide_thinking).await;
        runtime.messages = messages;
        runtime.cache_entries = cache_entries;
        runtime.persisted_until = runtime.messages.len();
    }

    let (transcript_scroll_view, document_container) =
        it::new_interactive_document_scroll_view(&transcript_md);
    let transcript_view = Arc::new(Mutex::new(InteractiveTranscriptView::new(
        Arc::clone(&mermaid_mode),
        Arc::clone(&mermaid_streaming),
    )));
    let status_tail_view = Arc::new(Mutex::new(InteractiveTranscriptView::new(
        Arc::clone(&mermaid_mode),
        Arc::clone(&mermaid_streaming),
    )));
    // Pi mounts a startup header and loaded-resource list above the chat
    // transcript. Keep their expansion state shared with tool rendering so
    // one Ctrl+O gesture updates both the presentation and the transcript.
    let tool_output_expanded = Arc::new(AtomicBool::new(args.verbose));
    let startup_presentation = if args.verbose || !settings.get_quiet_startup() {
        Some(Arc::new(Mutex::new(
            it::startup::InteractiveStartupPresentation::new(
                crate::config::VERSION,
                &cwd,
                &agent_dir,
                args,
                &settings,
                &runtime.extension_resources,
                runtime.extensions.runner.extensions(),
                &runtime.extensions.errors,
                &runtime.prompt_templates,
                tool_output_expanded.load(Ordering::Acquire),
            ),
        )))
    } else {
        None
    };
    transcript_scroll_view
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .set_scrollbar(fullscreen_scrollbar_mode(
            settings.get_fullscreen_scrollbar(),
            use_alt_screen,
        ));
    let mut renderer = InteractiveRenderer::new(terminal.clone(), use_alt_screen);
    renderer.set_show_hardware_cursor(settings.get_show_hardware_cursor());
    renderer.set_clear_on_shrink(settings.get_clear_on_shrink());

    // Theme selectors and the file watcher update the process-global theme
    // from outside this loop. Bridge that callback into the render loop so a
    // live theme replacement refreshes markdown styling and invalidates the
    // differential frame without waiting for a mode restart.
    let theme_changed = Arc::new(AtomicBool::new(false));
    let theme_changed_callback = Arc::clone(&theme_changed);
    it::tui_theme::on_theme_change(Arc::new(move || {
        theme_changed_callback.store(true, Ordering::Release);
    }));

    let footer_text: Arc<Mutex<Text>> = Arc::new(Mutex::new(Text::new(String::new(), 0, 0, None)));
    // Only active status indicators occupy the dock. Ordinary status messages
    // are materialized as ordered children of the scrollable chat document.
    let status_text: Arc<Mutex<Text>> = Arc::new(Mutex::new(Text::new("", 1, 0, None)));

    let mut modal: Option<Modal> = None;
    let mut startup_resume_active = startup_resume_picker;
    let mut startup_resume_cancelled = false;
    let mut startup_cross_project_cancelled = false;
    if let Some(source) = cross_project_source {
        modal = Some(Modal::CrossProjectSession(Arc::new(Mutex::new(
            it::session_meta::CrossProjectSessionPrompt::new(source),
        ))));
    }
    if let Some(sessions) = startup_resume_sessions {
        let picker = it::session_picker_items(sessions);
        let current_session_path = runtime.session.get_metadata().await.path;
        let picker_state = it::session_meta::SessionPickerState::new(
            it::session_meta::session_picker_records(&picker),
            runtime.cwd.clone(),
            (!current_session_path.is_empty()).then_some(current_session_path),
        );
        modal = Some(Modal::Resume(Arc::new(Mutex::new(picker_state)), picker));
    }
    let mut status_banner = initial_status_banner;
    if let Some(changelog) = startup_changelog {
        status_banner = changelog;
    }
    let stream_buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let live_transcript = Arc::new(Mutex::new(InteractiveLiveTranscript::default()));
    // Keep one loader instance for the lifetime of the interactive loop. The
    // scene is rebuilt on every frame, so constructing it inside build_scene
    // would reset the upstream 80 ms spinner before it can advance.
    let pending_loader = Arc::new(Mutex::new(pi_tui::components::Loader::new("")));
    pending_loader
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .stop();
    renderer.attach_loader_repaint(
        &mut pending_loader
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    let mut streaming = false;
    let mut llama_operation: Option<InteractiveLlamaOperation> = None;
    let mut bash_operation: Option<InteractiveBashOperation> = None;
    let mut pending_text = String::new();
    let mut easter_egg_components: Vec<SharedComponent> = Vec::new();
    let mut easter_egg_animation_until: Option<std::time::Instant> = None;
    let mut hidden_component_boundary_pending = false;
    let mut last_ctrl_c: Option<std::time::Instant> = None;
    let mut last_escape: Option<std::time::Instant> = None;
    // The parent process cannot change its working directory from a child
    // tool, so the branch is stable for this session. Resolve it once rather
    // than spawning `git` from the composer/render hot path.
    let footer_branch = footer::git_branch(&cwd);
    let mut last_composed_transcript: Option<String> = None;
    let mut last_status_text: Option<String> = None;
    let mut last_footer_text: Option<String> = None;
    let mut status_log = InteractiveStatusLog::default();
    let mut last_editor_border_state: Option<(String, bool)> = None;
    let mut cached_transcript_key: Option<InteractiveTranscriptRenderKey> = None;
    let mut cached_document_shape: Option<InteractiveDocumentShape> = None;
    let mut cached_scene_shape: Option<InteractiveSceneShape> = None;
    let mut cached_scene: Option<Arc<Mutex<Scene>>> = None;
    let mut footer_invalidation_generation = 0_u64;
    let mut cached_footer_key: Option<InteractiveFooterRenderKey> = None;

    if let Some(active_modal) = modal.as_mut() {
        renderer.focus(modal_shared(active_modal));
    } else {
        renderer.focus(editor.clone());
    }
    renderer.invalidate();
    renderer.query_cell_size();
    let mut input = InteractiveInputReader::start(renderer.terminal_handle());
    let mut skip_owner_render_after_immediate_repaint = false;

    let result = tokio::time::timeout(std::time::Duration::from_secs(24 * 60 * 60), async {
        loop {
            // Mermaid's upstream transformer receives the live assistant
            // stream state. Keep that context in one atomic flag so retained
            // Markdown children can switch between streaming and final
            // rendering without being rebuilt for every provider delta.
            mermaid_streaming.store(streaming, Ordering::Release);
            // Pi's TUI dispatches focused keyboard input and schedules an
            // immediate next-tick repaint before the next full owner pass.
            // The Rust owner is also responsible for transcript/footer
            // preparation, so consume only already-queued plain text here.
            // Control, resize, viewport, modal, and streaming events remain
            // on the normal ordered dispatcher below.
            if modal.is_none()
                && !streaming
                && llama_operation.is_none()
                && bash_operation.is_none()
                && cached_scene.is_some()
            {
                let immediate_keys = input.take_immediate_editor_keys();
                if !immediate_keys.is_empty() {
                    for key_str in immediate_keys {
                        let key = parse_key(&key_str);
                        apply_interactive_editor_input(&editor, &key_str, &key);
                    }
                    let bash_mode = editor.lock().unwrap_or_else(|error| error.into_inner()).starts_with_non_whitespace('!');
                    sync_editor_border(
                        &editor,
                        &thinking_level,
                        bash_mode,
                        &mut last_editor_border_state,
                    );
                    renderer.render_cached_scene(cached_scene.as_ref());
                    skip_owner_render_after_immediate_repaint = true;
                    continue;
                }
            }

            let mut owner_preparation_changed = false;
            if let Some(operation) = bash_operation.as_ref() {
                if operation.task.is_finished() {
                    let operation = bash_operation
                        .take()
                        .expect("bash operation was present at completion");
                    streaming = false;
                    pending_text.clear();
                    let capture = match operation.task.await {
                        Ok(Ok(capture)) => capture,
                        Ok(Err(error)) => {
                            status_banner = format!("bash failed: {error}");
                            *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = String::new();
                            continue;
                        }
                        Err(error) => {
                            status_banner = format!("bash task failed: {error}");
                            *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = String::new();
                            continue;
                        }
                    };
                    let message = bash_execution_message(
                        &operation.command,
                        &capture,
                        operation.exclude_from_context,
                    );
                    runtime.messages.push(message.clone());
                    runtime
                        .cache_entries
                        .extend(cache_entry_from_message(&message));
                    if let Err(error) = persist_messages_checked(
                        &mut runtime.session,
                        std::slice::from_ref(&message),
                    )
                    .await
                    {
                        status_banner = error;
                    } else {
                        runtime.persisted_until = runtime.messages.len();
                        status_banner = capture
                            .error_message
                            .as_deref()
                            .map(|error| format!("bash failed: {error}"))
                            .unwrap_or_default();
                    }
                    *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = String::new();
                } else {
                    let output = operation.output.lock().unwrap_or_else(|error| error.into_inner()).clone();
                    *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = render_live_bash_execution(
                        &operation.command,
                        &output,
                        operation.exclude_from_context,
                        it::messages::TranscriptRenderOptions {
                            hide_thinking,
                            show_images: settings.get_show_images(),
                            image_width_cells: settings.get_image_width_cells() as usize,
                            output_pad: settings.get_output_pad() as usize,
                            expand_tool_output: tool_output_expanded.load(Ordering::Acquire),
                        },
                    );
                    pending_text.clear();
                }
            }
            if let Some(operation) = llama_operation.as_ref() {
                if operation.task.is_finished() {
                    let operation = llama_operation
                        .take()
                        .expect("llama operation was present at completion");
                    pending_text.clear();
                    streaming = false;
                    let was_cancelled = operation.signal.load(Ordering::SeqCst);
                    let task_result = match operation.task.await {
                        Ok(result) => result,
                        Err(error) => Err(format!(
                            "llama.cpp operation task failed for {}: {error}",
                            operation.label
                        )),
                    };
                    if was_cancelled {
                        status_banner = "llama.cpp operation cancelled".to_string();
                    } else {
                        match task_result {
                            Ok(InteractiveLlamaOperationResult::Complete) => {
                                status_banner = match it::llama::LlamaManager::open(&runtime.models)
                                    .await
                                {
                                    Ok(_) => format!(
                                        "llama.cpp operation complete: {}",
                                        operation.label
                                    ),
                                    Err(error) => format!(
                                        "llama.cpp operation complete; refresh failed: {error}"
                                    ),
                                };
                            }
                            Ok(InteractiveLlamaOperationResult::Search(models)) => {
                                if models.is_empty() {
                                    status_banner = format!(
                                        "no Hugging Face GGUF models matched {}",
                                        operation.label
                                    );
                                } else {
                                    status_banner = format!(
                                        "Hugging Face results for {}",
                                        operation.label
                                    );
                                    modal = Some(Modal::HuggingFace(Arc::new(Mutex::new(
                                        it::llama::HuggingFaceSelector::new(models),
                                    ))));
                                }
                            }
                            Ok(InteractiveLlamaOperationResult::Details(details)) => {
                                if details.gated
                                    != crate::core::llama::HuggingFaceGated::NotGated
                                {
                                    status_banner = it::llama::huggingface_access_message(&details);
                                    modal = Some(Modal::HuggingFaceDownload(Arc::new(
                                        Mutex::new(
                                            it::llama::HuggingFaceDownloadSelector::access_gate(
                                                details,
                                            ),
                                        ),
                                    )));
                                } else if !details.quantizations.is_empty() {
                                    status_banner = format!(
                                        "select a quantization for {}",
                                        details.id
                                    );
                                    modal = Some(Modal::HuggingFaceDownload(Arc::new(
                                        Mutex::new(
                                            it::llama::HuggingFaceDownloadSelector::quantizations(
                                                details,
                                            ),
                                        ),
                                    )));
                                } else {
                                    match it::llama::client_for_models(&runtime.models) {
                                        Ok(client) => {
                                            llama_operation = Some(
                                                start_llama_download_operation(client, details.id),
                                            );
                                            streaming = true;
                                            status_banner =
                                                "llama.cpp download started".to_string();
                                        }
                                        Err(error) => status_banner = error,
                                    }
                                }
                            }
                            Err(error) => {
                                let progress = operation.progress.lock().unwrap_or_else(|error| error.into_inner()).clone();
                                status_banner = if progress.is_empty() {
                                    error
                                } else {
                                    format!("{progress}; {error}")
                                };
                            }
                        }
                    }
                } else {
                    let progress = operation.progress.lock().unwrap_or_else(|error| error.into_inner()).clone();
                    pending_text = if progress.is_empty() {
                        format!("llama.cpp: {} …", operation.label)
                    } else {
                        progress
                    };
                }
            }
            if easter_egg_animation_until
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
            {
                easter_egg_animation_until = None;
                // The component keeps its final frame in the document, but
                // the owner must paint that frame before stopping the timer
                // wakeups. This mirrors the upstream component's final
                // requestRender/dispose boundary.
                renderer.invalidate();
                owner_preparation_changed = true;
            }
            if theme_changed.swap(false, Ordering::Acquire) {
                transcript_md
                    .lock().unwrap_or_else(|error| error.into_inner())
                    .set_theme(it::tui_theme::markdown_theme());
                transcript_view.lock().unwrap_or_else(|error| error.into_inner()).invalidate_theme();
                renderer.invalidate();
                owner_preparation_changed = true;
            }

            let bash_mode = editor.lock().unwrap_or_else(|error| error.into_inner()).starts_with_non_whitespace('!');
            sync_editor_border(
                &editor,
                &thinking_level,
                bash_mode,
                &mut last_editor_border_state,
            );

            // Observe the scalar command/status slot only after active work
            // has settled. This turns ordinary showStatus calls into chat
            // children while keeping active indicators in the dock.
            status_log.observe_banner(
                &status_banner,
                runtime.messages.len(),
                streaming || llama_operation.is_some() || !pending_text.is_empty(),
            );
            if hidden_component_boundary_pending {
                status_log.mark_hidden_component_boundary();
                hidden_component_boundary_pending = false;
            }

            let output_pad = settings.get_output_pad() as usize;
            transcript_view.lock().unwrap_or_else(|error| error.into_inner()).set_output_pad(output_pad);
            let render_options = it::messages::TranscriptRenderOptions {
                hide_thinking,
                show_images: settings.get_show_images(),
                image_width_cells: settings.get_image_width_cells() as usize,
                output_pad,
                expand_tool_output: tool_output_expanded.load(Ordering::Acquire),
            };
            // Pi updates retained tool/assistant components when display
            // settings change mid-turn. The Rust live projection owns the
            // same render options as the persisted transcript; refresh it on
            // the owner loop so `/settings` changes take effect immediately
            // without waiting for another provider event.
            if streaming && bash_operation.is_none() && llama_operation.is_none() {
                let mut live = live_transcript.lock().unwrap_or_else(|error| error.into_inner());
                live.configure(render_options);
                let live_rendered = live.render();
                drop(live);
                if *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) != live_rendered {
                    *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = live_rendered;
                }
            }
            // Read the stream after the live projection has applied any
            // display-setting refresh so the same frame contains the newest
            // assistant/tool snapshot rather than waiting one loop tick.
            let stream = stream_buffer.lock().unwrap_or_else(|error| error.into_inner()).clone();
            let show_cache_miss_notices = settings.get_show_cache_miss_notices();
            let transcript_key = InteractiveTranscriptRenderKey {
                message_count: runtime.messages.len(),
                cache_entry_count: runtime.cache_entries.len(),
                stream: stream.clone(),
                status_revision: status_log.revision(),
                active_status: status_log.active_message().map(str::to_string),
                options: render_options,
                show_cache_miss_notices,
            };
            let transcript_changed = cached_transcript_key.as_ref() != Some(&transcript_key);
            if transcript_changed {
                owner_preparation_changed = true;
                let cache_notices = if show_cache_miss_notices {
                    cache_notice_timestamps(&runtime.cache_entries)
                } else {
                    Vec::new()
                };
                let transcript_blocks = build_interactive_transcript_blocks(
                    &runtime.messages,
                    render_options,
                    &stream,
                    &cache_notices,
                    &status_log,
                );
                let transcript_source = transcript_source_from_blocks(&transcript_blocks);
                if last_composed_transcript.as_deref() != Some(transcript_source.as_str()) {
                    transcript_md
                        .lock().unwrap_or_else(|error| error.into_inner())
                        .set_text(transcript_source.clone());
                    last_composed_transcript = Some(transcript_source);
                }
                transcript_view
                    .lock().unwrap_or_else(|error| error.into_inner())
                    .set_blocks(transcript_blocks);
                let status_tail_blocks = build_interactive_status_tail_blocks(&status_log);
                status_tail_view
                    .lock().unwrap_or_else(|error| error.into_inner())
                    .set_blocks(status_tail_blocks);
                cached_transcript_key = Some(transcript_key);
            }
            let has_status_tail = !status_log.tail_entries().is_empty();
            let document_shape = InteractiveDocumentShape {
                has_status_tail,
                easter_egg_ids: easter_egg_components
                    .iter()
                    .map(|component| Arc::as_ptr(component) as *const () as usize)
                    .collect(),
            };

            // Hidden components are ordinary document children in Pi. Keep
            // them inside the retained scroll view so a large animation or
            // repeated announcement cannot consume composer dock height.
            if cached_document_shape.as_ref() != Some(&document_shape) {
                owner_preparation_changed = true;
                let mut document = document_container.lock().unwrap_or_else(|error| error.into_inner());
                document.clear();
                if let Some(startup) = &startup_presentation {
                    document.add_child(startup.clone() as SharedComponent);
                }
                document.add_child(transcript_view.clone() as SharedComponent);
                for component in &easter_egg_components {
                    document.add_child(component.clone());
                }
                if has_status_tail {
                    document.add_child(status_tail_view.clone() as SharedComponent);
                }
                cached_document_shape = Some(document_shape);
            }
            // Only an active status indicator is rendered in the dock.
            {
                let rendered = status_log
                    .active_message()
                    .map(|message| it::tui_theme::fg("muted", message))
                    .unwrap_or_default();
                if last_status_text.as_deref() != Some(rendered.as_str()) {
                    owner_preparation_changed = true;
                    status_text.lock().unwrap_or_else(|error| error.into_inner()).set_text(rendered.clone());
                    last_status_text = Some(rendered);
                }
            }

            // 3) Footer.
            {
                let terminal_width = renderer.terminal_handle().lock().unwrap_or_else(|error| error.into_inner()).width();
                let extension_statuses = runtime.extensions.host.extension_statuses();
                // Auth is a scalar lookup, not a history-sized calculation;
                // include it in the key so a login/logout or OAuth refresh
                // updates the subscription marker without rebuilding usage
                // totals on every composer key.
                let using_subscription = runtime
                    .models
                    .check_auth(&runtime.provider)
                    .is_some_and(|auth| auth.auth_type == "oauth")
                    && runtime
                        .models
                        .get_provider(&runtime.provider)
                        .and_then(|provider| provider.auth.oauth)
                        .is_some_and(|oauth| oauth.is_subscription());
                let footer_key = InteractiveFooterRenderKey {
                    message_count: runtime.messages.len(),
                    cache_entry_count: runtime.cache_entries.len(),
                    session_name: runtime.session_name.clone(),
                    provider: runtime.provider.clone(),
                    model_id: runtime.model.id.clone(),
                    model_label: runtime.model.name.clone(),
                    thinking: thinking_level.clone(),
                    reasoning: runtime.model.reasoning,
                    context_window: runtime.model.context_window,
                    auto_compact: settings.get_compaction_enabled(),
                    terminal_width,
                    modal_id: modal_identity(modal.as_ref()),
                    using_subscription,
                    extension_statuses: extension_statuses.clone(),
                    invalidation_generation: footer_invalidation_generation,
                };
                if cached_footer_key.as_ref() != Some(&footer_key) {
                    owner_preparation_changed = true;
                    let (usage, cache_hit_rate) = footer_usage_from_entries(&runtime.cache_entries);
                    let context_tokens =
                        pi_agent::harness::compaction::estimate_context_tokens(&runtime.messages)
                            .tokens;
                    let provider_count = {
                        let mut providers = std::collections::BTreeSet::new();
                        for model in runtime.models.get_available(None) {
                            providers.insert(model.provider);
                        }
                        if providers.is_empty() {
                            1
                        } else {
                            providers.len()
                        }
                    };
                    let fd = FooterData {
                        cwd: cwd.clone(),
                        branch: footer_branch.clone(),
                        session_name: runtime.session_name.clone(),
                        model_id: Some(runtime.model.id.clone()),
                        model_provider: Some(runtime.provider.clone()),
                        using_subscription,
                        model_label: Some(runtime.model.name.clone()),
                        thinking: Some(thinking_level.clone()),
                        reasoning: runtime.model.reasoning,
                        provider_count,
                        context_tokens: Some(context_tokens),
                        context_window: runtime.model.context_window,
                        auto_compact: settings.get_compaction_enabled(),
                        usage,
                        cache_hit_rate,
                    };
                    let lines = footer::render_footer_with_extras(
                        &fd,
                        terminal_width,
                        &FooterExtras {
                            extension_statuses,
                            experimental_features: crate::core::experimental::are_enabled(),
                        },
                    );
                    let text = lines.join("\n");
                    if last_footer_text.as_deref() != Some(text.as_str()) {
                        footer_text.lock().unwrap_or_else(|error| error.into_inner()).set_text(text.clone());
                        last_footer_text = Some(text);
                    }
                    cached_footer_key = Some(footer_key);
                }
            }

            // 4) Scene.
            let scene_shape = InteractiveSceneShape {
                modal_id: modal_identity(modal.as_ref()),
                pending_text: pending_text.clone(),
            };
            if cached_scene_shape.as_ref() != Some(&scene_shape) {
                owner_preparation_changed = true;
                let modal_comp: Option<SharedComponent> = match modal.as_mut() {
                    Some(m) => Some(modal_shared(m)),
                    None => None,
                };
                let scene = it::build_interactive_scene_with_loader_and_scroll_view(
                    &transcript_scroll_view,
                    &editor,
                    &footer_text,
                    Some(&status_text),
                    modal_comp,
                    &[],
                    &pending_loader,
                    &pending_text,
                );
                cached_scene = Some(scene);
                cached_scene_shape = Some(scene_shape);
            }
            let scene = cached_scene
                .as_ref()
                .expect("interactive scene cache initialized")
                .clone();
            let can_reuse_immediate_repaint = skip_owner_render_after_immediate_repaint
                && !owner_preparation_changed
                && easter_egg_animation_until.is_none()
                && !renderer.has_pending_render_request();
            skip_owner_render_after_immediate_repaint = false;
            if !can_reuse_immediate_repaint {
                renderer.render_scene(&scene);
            }

            // Upstream's startup benchmark initializes and renders the real
            // TUI, gives terminal capability probes a short window to settle,
            // then restores the terminal without waiting for user input.
            if crate::config::env_flag("PI_STARTUP_BENCHMARK") {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                return Ok(());
            }

            // 4) Input.
            let render_wakeup = renderer.render_wakeup();
            let extension_status_wakeup = runtime.extensions.host.extension_status_wakeup();
            let ev = tokio::select! {
                event = input.recv() => {
                    event.ok_or_else(|| "terminal input reader stopped".to_string())??
                },
                _ = wait_for_render_wakeup(render_wakeup) => {
                    // A retained component (for example scrollbar auto-hide)
                    // requested a repaint from a timer thread. Rendering stays
                    // on this owner task; the notification only wakes it.
                    continue;
                },
                _ = extension_status_wakeup.notified() => {
                    // Extension status rows live in the footer rather than
                    // the general UI request queue, so repaint directly when
                    // a worker changes one without waiting for a keypress.
                    renderer.invalidate();
                    continue;
                },
                Some(_) = shutdown_signals.recv() => {
                    if let Some(operation) = bash_operation.as_ref() {
                        operation.signal.store(true, Ordering::SeqCst);
                    }
                    if let Some(operation) = llama_operation.as_ref() {
                        operation.signal.store(true, Ordering::SeqCst);
                    }
                    // Upstream disposes extensions from its signal path before
                    // restoring the terminal. Drop remains idempotent for the
                    // normal loop-exit path below.
                    runtime.shutdown_extensions("signal");
                    break Ok(());
                },
                _ = tokio::time::sleep(TUI_MIN_RENDER_INTERVAL),
                    if easter_egg_animation_until.is_some()
                        || llama_operation.is_some()
                        || bash_operation.is_some() => {
                    continue;
                }
            };
            let key_str = match ev {
                pi_tui::terminal::TerminalEvent::Key(k) => k,
                pi_tui::terminal::TerminalEvent::Resize(_w, h) => {
                    renderer.invalidate();
                    editor.lock().unwrap_or_else(|error| error.into_inner()).set_terminal_rows(h as usize);
                    continue;
                }
            };
            if key_str.is_empty() {
                continue;
            }
            if renderer.consume_cell_size_response(&key_str) {
                continue;
            }
            // Kitty emits a press and release CSI-u event. The interactive
            // handlers act on presses; dispatching the release would apply
            // navigation a second time.
            if is_key_release(&key_str) {
                continue;
            }
            // A bash task can finish after the loop's completion check but
            // before the input branch wins `tokio::select!`. Keep the key in
            // the reader's queue so the next iteration settles bash first;
            // otherwise the operation guard below would drop a prompt typed
            // immediately after `!command`.
            if bash_operation
                .as_ref()
                .is_some_and(|operation| operation.task.is_finished())
            {
                defer_input_until_bash_completion(&mut input.pending_events, key_str);
                continue;
            }
            if modal.is_none() && renderer.dispatch_viewport_input(&key_str) {
                continue;
            }
            let key = parse_key(&key_str);
            if modal.is_some() && is_printable_input_batch(&key_str, &key) {
                enqueue_modal_printable_batch(&mut input.pending_events, &key_str);
                continue;
            }

            if key.ctrl && key.base == "z" && !key.alt && !key.shift {
                #[cfg(unix)]
                {
                    let terminal = renderer.terminal_handle();
                    match suspend_interactive(
                        &terminal,
                        &mut input,
                        use_alt_screen,
                        &mut sigcont,
                    )
                    .await
                    {
                        Ok(()) => {
                            renderer.invalidate();
                        }
                        Err(error) => {
                            status_banner = format!("suspend failed: {error}");
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    // Match upstream's documented unsupported behavior on
                    // platforms without Unix process groups.
                    status_banner = "Suspend to background is not supported on Windows".to_string();
                }
                continue;
            }

            if modal.is_none() && key.ctrl && key.base == "o" && !key.alt && !key.shift {
                let expanded = !tool_output_expanded.fetch_xor(true, Ordering::AcqRel);
                if let Some(startup) = &startup_presentation {
                    startup.lock().unwrap_or_else(|error| error.into_inner()).set_expanded(expanded);
                }
                // Expanding the startup/help component increases the retained
                // document height. Keep Pi's `follow: "end"` behavior
                // explicit here so the fixed composer/footer remain visible
                // after the document grows beyond the terminal viewport.
                transcript_scroll_view.lock().unwrap_or_else(|error| error.into_inner()).scroll_to_end();
                live_transcript.lock().unwrap_or_else(|error| error.into_inner()).configure(
                    it::messages::TranscriptRenderOptions {
                        hide_thinking,
                        show_images: settings.get_show_images(),
                        image_width_cells: settings.get_image_width_cells() as usize,
                        output_pad: settings.get_output_pad() as usize,
                        expand_tool_output: expanded,
                    },
                );
                status_banner = format!(
                    "Tool output: {}",
                    if expanded { "expanded" } else { "collapsed" }
                );
                renderer.invalidate();
                continue;
            }

            if key.ctrl && key.base == "c" {
                if streaming {
                    if let Some(operation) = bash_operation.as_ref() {
                        operation.signal.store(true, Ordering::SeqCst);
                        status_banner = "cancelling bash command…".to_string();
                        continue;
                    }
                    if let Some(operation) = llama_operation.as_ref() {
                        operation.signal.store(true, Ordering::SeqCst);
                        status_banner = "cancelling llama.cpp operation…".to_string();
                        continue;
                    }
                    status_banner = "Press Ctrl+C again to quit".to_string();
                    continue;
                }
                let now = std::time::Instant::now();
                let draft_is_empty = editor.lock().unwrap_or_else(|error| error.into_inner()).is_empty();
                if last_ctrl_c.is_some_and(|previous| {
                    now.duration_since(previous) <= std::time::Duration::from_millis(500)
                }) {
                    return Ok(());
                }
                // Pi clears the editor on the first Ctrl+C without creating a
                // status message, even when it was already empty. A second
                // press inside the 500 ms window exits the session.
                if !draft_is_empty {
                    editor.lock().unwrap_or_else(|error| error.into_inner()).set_text("");
                }
                last_ctrl_c = Some(now);
                continue;
            }
            if let Some(operation) = bash_operation.as_ref() {
                if key.base == "esc" || key.base == "escape" {
                    operation.signal.store(true, Ordering::SeqCst);
                    status_banner = "cancelling bash command…".to_string();
                }
                // Do not let editor input accumulate while a direct bash
                // command owns the interactive turn.
                continue;
            }
            if let Some(operation) = llama_operation.as_ref() {
                if key.base == "esc" || key.base == "escape" {
                    operation.signal.store(true, Ordering::SeqCst);
                    status_banner = "cancelling llama.cpp operation…".to_string();
                }
                // Do not let editor input accumulate while a model operation
                // owns the interactive turn; only cancellation and redraw
                // events are meaningful until the real HTTP request settles.
                continue;
            }
            last_ctrl_c = None;
            let is_plain_escape = (key.base == "esc" || key.base == "escape")
                && !key.ctrl
                && !key.shift
                && !key.alt
                && !key.super_key;
            let editor_is_empty = if is_plain_escape || (key.ctrl && key.base == "d") {
                editor.lock().unwrap_or_else(|error| error.into_inner()).is_empty()
            } else {
                false
            };
            let can_arm_double_escape =
                modal.is_none() && !streaming && is_plain_escape && editor_is_empty;
            if !can_arm_double_escape {
                last_escape = None;
            }
            if should_exit_on_key(&key, if editor_is_empty { "" } else { "draft" }) {
                return Ok(());
            }

            // The editor's Escape hook is consumed for an empty line. Pi
            // opens the configured tree/fork selector only on a second plain
            // Escape received within 500 ms.
            if can_arm_double_escape {
                let action = resolve_double_escape(
                    settings.get_double_escape_action(),
                    &mut last_escape,
                    std::time::Instant::now(),
                );
                match action {
                    Some(DoubleEscapeAction::Tree) => {
                        let terminal_height = renderer
                            .terminal_handle()
                            .lock().unwrap_or_else(|error| error.into_inner())
                            .height();
                        match tree_selector_for_session(
                            &runtime.session,
                            terminal_height,
                            it::tree_selector::TreeFilterMode::from_setting(
                                settings.get_tree_filter_mode(),
                            ),
                        )
                        .await
                        {
                            Err(error) => status_banner = error.to_string(),
                            Ok(None) => status_banner = "No entries in session".to_string(),
                            Ok(Some(selector)) => {
                                modal = Some(Modal::Tree(Arc::new(Mutex::new(selector))));
                            }
                        }
                    }
                    Some(DoubleEscapeAction::Fork) => {
                        if !runtime.session_persistence {
                            status_banner =
                                "fork requires a persistent session; remove --no-session".to_string();
                        } else {
                            let items = fork_selector_items(&runtime.session).await;
                            if items.is_empty() {
                                status_banner = "No messages to fork from".to_string();
                            } else {
                                let last = items.len().saturating_sub(1);
                                let mut selector = ListSelector::new_slash_layout(items, 10);
                                selector.set_selected_index(last);
                                modal = Some(Modal::Fork(Arc::new(Mutex::new(selector))));
                            }
                        }
                    }
                    None => {}
                }
                continue;
            }

            if modal.is_none() && !streaming && key.ctrl && key.base == "p" {
                let cycled_model = cycle_scoped_model(&mut runtime);
                if cycled_model.is_some()
                    && maybe_add_daxnuts_component(
                        &runtime,
                        &mut easter_egg_components,
                        &mut easter_egg_animation_until,
                    )
                {
                    hidden_component_boundary_pending = true;
                }
                status_banner = cycled_model.unwrap_or_else(|| {
                    if runtime.scoped_models.is_empty() {
                        "No scoped models configured; use /scoped-models first".to_string()
                    } else {
                        "Only one scoped model configured".to_string()
                    }
                });
                _session_environment.set_model(&runtime.provider, &runtime.model.name);
                let next_thinking = settings
                    .get_model_thinking_level(&runtime.provider, &runtime.model.id)
                    .map(str::to_string)
                    .or_else(|| settings.get_default_thinking_level().map(str::to_string))
                    .unwrap_or_else(|| {
                        crate::core::model_resolver::DEFAULT_THINKING_LEVEL.to_string()
                    });
                thinking_level = next_thinking;
                hide_thinking = thinking_level == "off" || settings.get_hide_thinking_block();
                _session_environment.set_reasoning_level(&thinking_level);
                continue;
            }

            // Modal input handling.
            if let Some(active_modal) = &mut modal {
                let mut close_modal = false;
                match active_modal {
                    Modal::Model(sel) => {
                        let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                        let (selected_model, persist, should_close) = match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx))
                                if idx < guard.count() => (guard.selected_model(), false, true),
                            it::selectors::SelectorAction::SelectAsDefault(Some(idx))
                                if idx < guard.count() => (guard.selected_model(), true, true),
                            it::selectors::SelectorAction::Cancel
                            | it::selectors::SelectorAction::Select(_)
                            | it::selectors::SelectorAction::SelectAsDefault(_) =>
                                (None, false, true),
                            _ => (None, false, false),
                        };
                        if let Some(model) = selected_model {
                            let value = format!("{}/{}", model.provider, model.id);
                            let selection = if persist {
                                it::apply_model_selection(&mut settings, &value)
                            } else {
                                it::parse_model_selection(&value)
                            };
                            if let Some((provider, id)) = selection {
                                invalidate_interactive_harness(&mut runtime);
                                runtime.provider = provider.clone();
                                if let Some(m) = runtime.models.get_model(&provider, &id) {
                                    runtime.model = m;
                                }
                                let next_thinking = settings
                                    .get_model_thinking_level(&runtime.provider, &runtime.model.id)
                                    .map(str::to_string)
                                    .or_else(|| {
                                        settings.get_default_thinking_level().map(str::to_string)
                                    })
                                    .unwrap_or_else(|| {
                                        crate::core::model_resolver::DEFAULT_THINKING_LEVEL
                                            .to_string()
                                    });
                                thinking_level = next_thinking;
                                hide_thinking = thinking_level == "off"
                                    || settings.get_hide_thinking_block();
                                _session_environment
                                    .set_model(&runtime.provider, &runtime.model.name);
                                _session_environment.set_reasoning_level(&thinking_level);
                                status_banner = if persist {
                                    format!("Default model: {provider}/{id}")
                                } else {
                                    format!("Model: {}", runtime.model.id)
                                };
                                if maybe_add_daxnuts_component(
                                    &runtime,
                                    &mut easter_egg_components,
                                    &mut easter_egg_animation_until,
                                ) {
                                    hidden_component_boundary_pending = true;
                                }
                            }
                        }
                        if should_close {
                            guard.dispose();
                            close_modal = true;
                        }
                    }
                    Modal::Llama(sel) => {
                        let action = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            match guard.handle(&key) {
                                it::llama::LlamaSelectorAction::Select(action) => Some(action),
                                it::llama::LlamaSelectorAction::Cancel => {
                                    close_modal = true;
                                    None
                                }
                                it::llama::LlamaSelectorAction::None => None,
                            }
                        };
                        if let Some(action) = action {
                            close_modal = true;
                            match action {
                                crate::core::llama::LlamaManagerAction::Close => {}
                                crate::core::llama::LlamaManagerAction::Download => {
                                    // The editor is the same cancellable text
                                    // surface used by Pi's download prompt;
                                    // submitting the completed spec re-enters
                                    // this command and calls the real router.
                                    editor
                                        .lock().unwrap_or_else(|error| error.into_inner())
                                        .set_text("/llama download ");
                                    status_banner =
                                        "enter <repo>:<quantization>, then press Enter".to_string();
                                }
                                crate::core::llama::LlamaManagerAction::Model {
                                    id,
                                    action: model_action,
                                } => match model_action {
                                    crate::core::llama::LlamaModelAction::Load => {
                                        match it::llama::LlamaManager::open(&runtime.models).await {
                                            Ok(manager) => {
                                                let loaded = it::llama::loaded_model_ids(&manager.catalog)
                                                    .into_iter()
                                                    .filter(|model| model != &id)
                                                    .collect::<Vec<_>>();
                                                if loaded.is_empty() {
                                                    llama_operation = Some(start_llama_load_operation(
                                                        manager.client,
                                                        id,
                                                        loaded,
                                                        false,
                                                    ));
                                                    streaming = true;
                                                    status_banner = "llama.cpp load started".to_string();
                                                } else {
                                                    modal = Some(Modal::LlamaLoadPlan {
                                                        selector: Arc::new(Mutex::new(
                                                            it::llama::LlamaLoadPlanSelector::new(
                                                                &id, &loaded,
                                                            ),
                                                        )),
                                                        client: manager.client,
                                                        target: id,
                                                        loaded,
                                                    });
                                                    close_modal = false;
                                                    status_banner =
                                                        "other llama.cpp models are loaded; choose a load plan"
                                                            .to_string();
                                                }
                                            }
                                            Err(error) => status_banner = error,
                                        }
                                    }
                                    crate::core::llama::LlamaModelAction::Unload => {
                                        match it::llama::client_for_models(&runtime.models) {
                                            Ok(client) => {
                                                modal = Some(Modal::LlamaUnloadConfirm {
                                                    selector: Arc::new(Mutex::new(
                                                        it::llama::LlamaUnloadConfirmSelector::new(&id),
                                                    )),
                                                    client,
                                                    target: id,
                                                });
                                                close_modal = false;
                                                status_banner = "confirm llama.cpp unload".to_string();
                                            }
                                            Err(error) => status_banner = error,
                                        }
                                    }
                                    crate::core::llama::LlamaModelAction::Observe => {
                                        status_banner = format!(
                                            "llama.cpp model {id} is still loading or downloading"
                                        );
                                    }
                                },
                            }
                        }
                    }
                    Modal::LlamaLoadPlan {
                        selector,
                        client,
                        target,
                        loaded,
                    } => {
                        let action = {
                            let mut guard = selector.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        if let Some(action) = action {
                            close_modal = true;
                            match action {
                                it::llama::LlamaLoadPlanAction::UnloadAll => {
                                    llama_operation = Some(start_llama_load_operation(
                                        client.clone(),
                                        target.clone(),
                                        loaded.clone(),
                                        true,
                                    ));
                                    streaming = true;
                                    status_banner = "llama.cpp replacing loaded models".to_string();
                                }
                                it::llama::LlamaLoadPlanAction::KeepLoaded => {
                                    llama_operation = Some(start_llama_load_operation(
                                        client.clone(),
                                        target.clone(),
                                        loaded.clone(),
                                        false,
                                    ));
                                    streaming = true;
                                    status_banner = "llama.cpp loading alongside resident models".to_string();
                                }
                                it::llama::LlamaLoadPlanAction::Cancel => {}
                            }
                        }
                    }
                    Modal::LlamaUnloadConfirm {
                        selector,
                        client,
                        target,
                    } => {
                        let confirmed = {
                            let mut guard = selector.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        if let Some(confirmed) = confirmed {
                            close_modal = true;
                            if confirmed {
                                llama_operation = Some(start_llama_model_operation(
                                    client.clone(),
                                    crate::core::llama::LlamaManagerAction::Model {
                                        id: target.clone(),
                                        action: crate::core::llama::LlamaModelAction::Unload,
                                    },
                                ));
                                streaming = true;
                                status_banner = "llama.cpp unload started".to_string();
                            }
                        }
                    }
                    Modal::HuggingFace(sel) => {
                        let selected = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        if let Some(result) = selected {
                            close_modal = true;
                            if let Ok(model) = result {
                                match start_huggingface_details_operation(model.id.clone()) {
                                    Ok(operation) => {
                                        llama_operation = Some(operation);
                                        streaming = true;
                                        status_banner = format!(
                                            "loading Hugging Face details for {}",
                                            model.id
                                        );
                                    }
                                    Err(error) => status_banner = error,
                                }
                            }
                        }
                    }
                    Modal::HuggingFaceDownload(sel) => {
                        let selected = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        if let Some(action) = selected {
                            match action {
                                it::llama::HuggingFaceDownloadAction::Continue(details) => {
                                    if details.quantizations.is_empty() {
                                        match it::llama::client_for_models(&runtime.models) {
                                            Ok(client) => {
                                                llama_operation = Some(
                                                    start_llama_download_operation(client, details.id),
                                                );
                                                streaming = true;
                                                status_banner =
                                                    "llama.cpp download started".to_string();
                                                close_modal = true;
                                            }
                                            Err(error) => status_banner = error,
                                        }
                                    } else {
                                        status_banner = format!(
                                            "select a quantization for {}",
                                            details.id
                                        );
                                        modal = Some(Modal::HuggingFaceDownload(Arc::new(
                                            Mutex::new(
                                                it::llama::HuggingFaceDownloadSelector::quantizations(
                                                    details,
                                                ),
                                            ),
                                        )));
                                    }
                                }
                                it::llama::HuggingFaceDownloadAction::Download(spec) => {
                                    match it::llama::client_for_models(&runtime.models) {
                                        Ok(client) => {
                                            llama_operation = Some(
                                                start_llama_download_operation(client, spec),
                                            );
                                            streaming = true;
                                            status_banner =
                                                "llama.cpp download started".to_string();
                                            close_modal = true;
                                        }
                                        Err(error) => status_banner = error,
                                    }
                                }
                                it::llama::HuggingFaceDownloadAction::Cancel => {
                                    close_modal = true;
                                }
                            }
                        }
                    }
                    Modal::ScopedModels(sel) => {
                        let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                        match guard.handle(&key) {
                            it::selectors::ScopedModelsAction::Toggle { model, enabled } => {
                                status_banner = format!(
                                    "{} {}",
                                    if enabled { "Enabled" } else { "Disabled" },
                                    model
                                );
                            }
                            it::selectors::ScopedModelsAction::Cancel => {
                                runtime.scoped_models = guard.selected_models();
                                status_banner = if runtime.scoped_models.is_empty() {
                                    "Scoped model cycling disabled".to_string()
                                } else {
                                    format!(
                                        "Scoped models: {}",
                                        runtime.scoped_models.join(", ")
                                    )
                                };
                                close_modal = true;
                            }
                            it::selectors::ScopedModelsAction::None => {}
                        }
                    }
                    Modal::Thinking(sel) => {
                        let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                        let (selected_item, persist, should_close) = match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx))
                                if idx < guard.count() => (guard.selected_item(), false, true),
                            it::selectors::SelectorAction::SelectAsDefault(Some(idx))
                                if idx < guard.count() => (guard.selected_item(), true, true),
                            it::selectors::SelectorAction::Cancel
                            | it::selectors::SelectorAction::Select(_)
                            | it::selectors::SelectorAction::SelectAsDefault(_) =>
                                (None, false, true),
                            _ => (None, false, false),
                        };
                        if let Some(item) = selected_item {
                            invalidate_interactive_harness(&mut runtime);
                            if persist {
                                settings.set_default_thinking_level(&item.value);
                            }
                            thinking_level = item.value.clone();
                            // The explicit /thinking choice changes the
                            // generation level, not the independent
                            // hide-thinking setting. Preserve that setting
                            // for non-off levels, matching the settings
                            // callback and the upstream session state.
                            hide_thinking = hide_thinking_for_level(&settings, &item.value);
                            _session_environment.set_reasoning_level(&thinking_level);
                            status_banner = if persist {
                                format!("Default thinking level: {}", item.value)
                            } else {
                                format!("Thinking level: {}", item.value)
                            };
                        }
                        if should_close {
                            close_modal = true;
                        }
                    }
                    Modal::Theme(sel) => {
                        let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    match load_interactive_theme_checked(&item.value) {
                                        Ok(()) => {
                                            status_banner = format!("Theme: {}", item.value)
                                        }
                                        Err(error) => {
                                            status_banner = format!("Theme failed: {error}")
                                        }
                                    }
                                }
                                close_modal = true;
                            }
                            it::selectors::SelectorAction::Cancel | it::selectors::SelectorAction::Select(_) => {
                                close_modal = true;
                            }
                            _ => {}
                        }
                    }
                    Modal::Fork(sel) => {
                        let selected_entry_id = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            match guard.handle(&key) {
                                it::selectors::SelectorAction::Select(Some(idx))
                                    if idx < guard.count() => guard.selected_item().map(|item| item.value),
                                it::selectors::SelectorAction::Cancel
                                | it::selectors::SelectorAction::Select(_) => None,
                                _ => None,
                            }
                        };
                        close_modal = true;
                        if let Some(entry_id) = selected_entry_id {
                            let result = execute_interactive_fork(
                                &mut runtime,
                                "fork",
                                entry_id,
                                ForkPosition::Before,
                                InteractiveForkContext {
                                    settings: &settings,
                                    thinking_level: &thinking_level,
                                    transcript_md: &transcript_md,
                                    hide_thinking,
                                },
                                None,
                            )
                            .await;
                            refresh_startup_presentation(
                                startup_presentation.as_ref(),
                                &runtime,
                                args,
                                &settings,
                            );
                            if let Some(text) = result.editor_text {
                                editor.lock().unwrap_or_else(|error| error.into_inner()).set_text(&text);
                            }
                            status_log.clear();
                            status_banner = result.status;
                        }
                    }
                    Modal::Resume(sel, sessions) => {
                        let action = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        let (mut close_resume, selected_session_path) = match action {
                            it::session_meta::SessionPickerAction::Select { path, .. } => {
                                (true, Some(path))
                            }
                            it::session_meta::SessionPickerAction::Cancel => (true, None),
                            it::session_meta::SessionPickerAction::DeleteCurrentDenied(message) => {
                                status_banner = message;
                                (false, None)
                            }
                            it::session_meta::SessionPickerAction::DeleteRequested(_)
                            | it::session_meta::SessionPickerAction::DeleteCancelled
                            | it::session_meta::SessionPickerAction::None
                            | it::session_meta::SessionPickerAction::ScopeChanged(_)
                            | it::session_meta::SessionPickerAction::SortChanged(_)
                            | it::session_meta::SessionPickerAction::NameFilterChanged(_)
                            | it::session_meta::SessionPickerAction::PathVisibilityChanged(_)
                            | it::session_meta::SessionPickerAction::BeginRename(_) => (false, None),
                            it::session_meta::SessionPickerAction::DeleteConfirmed(path) => {
                                if let Some(index) =
                                    sessions.iter().position(|session| session.metadata.path == path)
                                {
                                    match runtime.repo.delete(&sessions[index].metadata).await {
                                        Ok(()) => {
                                            status_banner = format!(
                                                "deleted session {}",
                                                sessions[index].id.get(..8).unwrap_or(&sessions[index].id)
                                            );
                                            sessions.remove(index);
                                            sel.lock().unwrap_or_else(|error| error.into_inner()).set_sessions(
                                                it::session_meta::session_picker_records(sessions),
                                            );
                                        }
                                        Err(error) => {
                                            status_banner = format!("delete session failed: {error}");
                                        }
                                    }
                                }
                                (false, None)
                            }
                        };
                        if startup_resume_active
                            && close_resume
                            && selected_session_path.is_none()
                        {
                            startup_resume_cancelled = true;
                        }
                        if let Some(session_path) = selected_session_path {
                            if let Some(meta) =
                                sessions.iter().find(|s| s.metadata.path == session_path)
                            {
                                // Refuse to resume a session whose stored cwd
                                // no longer exists (upstream session-cwd.ts).
                                let cwd_now = std::env::current_dir()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let issue = crate::core::session_cwd::get_missing_session_cwd_issue(
                                    Some(&meta.metadata.path),
                                    &meta.metadata.cwd,
                                    &cwd_now,
                                );
                                if let Some(issue) = issue {
                                    status_banner = crate::core::session_cwd::format_missing_session_cwd_error(&issue);
                                    close_resume = !startup_resume_active;
                                } else if !session_switch_allowed(
                                    &runtime,
                                    "resume",
                                    Some(&meta.metadata.path),
                                ) {
                                    status_banner = "resume cancelled by extension".to_string();
                                } else {
                                    match runtime.repo.open(&meta.metadata).await {
                                        Ok(session) => {
                                            let previous_session_file =
                                                runtime.session.get_metadata().await.path;
                                            let target_session_file = session.get_metadata().await.path;
                                            let selected_session_name = session.get_name().await;
                                            shutdown_extensions_before_session_replace(
                                                &runtime,
                                                "resume",
                                                Some(&target_session_file),
                                            );
                                            invalidate_interactive_harness(&mut runtime);
                                            runtime.session = session;
                                            runtime.session_id = meta.id.clone();
                                            runtime.session_name = selected_session_name;
                                            startup_resume_active = false;
                                            let (messages, cache_entries) =
                                                rehydrate_transcript(&runtime, &transcript_md, hide_thinking).await;
                                            runtime.messages = messages;
                                            runtime.cache_entries = cache_entries;
                                            runtime.persisted_until = runtime.messages.len();
                                            status_log.clear();
                                            clear_easter_egg_components(
                                                &mut easter_egg_components,
                                                &mut easter_egg_animation_until,
                                            );
                                            let notes = replace_extensions(
                                                &mut runtime,
                                                &settings,
                                                &thinking_level,
                                                "resume",
                                                Some(&previous_session_file),
                                                Some(&target_session_file),
                                            );
                                            refresh_startup_presentation(
                                                startup_presentation.as_ref(),
                                                &runtime,
                                                args,
                                                &settings,
                                            );
                                            status_banner = format!(
                                                "resumed session {} ({} prior messages)",
                                                meta.id.get(..8).unwrap_or(&meta.id),
                                                runtime.messages.len()
                                            );
                                            if !notes.is_empty() {
                                                status_banner.push_str(&format!(
                                                    " (extensions: {})",
                                                    notes.join("; ")
                                                ));
                                            }
                                        }
                                        Err(e) => {
                                            status_banner = format!("resume failed: {e}");
                                            close_resume = !startup_resume_active;
                                        }
                                    }
                                }
                            } else {
                                // A file may disappear after the inventory is
                                // rendered. Keep startup `--resume` open so
                                // the user can retry or cancel instead of
                                // silently falling back to a new session.
                                status_banner = format!(
                                    "resume failed: session {} no longer exists",
                                    session_path
                                );
                                close_resume = !startup_resume_active;
                            }
                        }
                        if close_resume {
                            close_modal = true;
                        }
                    }
                    Modal::CrossProjectSession(prompt) => {
                        let (action, source) = {
                            let mut guard = prompt.lock().unwrap_or_else(|error| error.into_inner());
                            let action = guard.handle(&key);
                            (action, guard.source().clone())
                        };
                        match action {
                            it::session_meta::CrossProjectSessionAction::Confirm => {
                                if !session_switch_allowed(&runtime, "fork", Some(&source.path)) {
                                    status_banner = "fork cancelled by extension".to_string();
                                } else {
                                    let new_id = args
                                        .session_id
                                        .clone()
                                        .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
                                        .unwrap_or_else(pi_agent::session::new_id);
                                    match runtime
                                    .repo
                                    .fork(
                                        &source,
                                        CreateOptions {
                                            id: Some(new_id.clone()),
                                            cwd: cwd.clone(),
                                            parent_session_id: None,
                                            metadata: None,
                                            fork_options: ForkOptions::Tree,
                                        },
                                    )
                                    .await
                                    {
                                    Ok(session) => {
                                        let previous_session_file =
                                            runtime.session.get_metadata().await.path;
                                        let target_session_file = session.get_metadata().await.path;
                                        shutdown_extensions_before_session_replace(
                                            &runtime,
                                            "fork",
                                            Some(&target_session_file),
                                        );
                                        invalidate_interactive_harness(&mut runtime);
                                        runtime.session = session;
                                        runtime.session_id = new_id.clone();
                                        runtime.session_name = runtime.session.get_name().await;
                                        let (messages, cache_entries) = rehydrate_transcript(
                                            &runtime,
                                            &transcript_md,
                                            hide_thinking,
                                        )
                                        .await;
                                        runtime.messages = messages;
                                        runtime.cache_entries = cache_entries;
                                        runtime.persisted_until = runtime.messages.len();
                                        status_log.clear();
                                        clear_easter_egg_components(
                                            &mut easter_egg_components,
                                            &mut easter_egg_animation_until,
                                        );
                                        let notes = replace_extensions(
                                            &mut runtime,
                                            &settings,
                                            &thinking_level,
                                            "fork",
                                            Some(&previous_session_file),
                                            Some(&target_session_file),
                                        );
                                        refresh_startup_presentation(
                                            startup_presentation.as_ref(),
                                            &runtime,
                                            args,
                                            &settings,
                                        );
                                        status_banner = format!(
                                            "forked session {} into {}",
                                            source.id.get(..8).unwrap_or(&source.id),
                                            new_id.get(..8).unwrap_or(&new_id)
                                        );
                                        if !notes.is_empty() {
                                            status_banner.push_str(&format!(
                                                " (extensions: {})",
                                                notes.join("; ")
                                            ));
                                        }
                                        close_modal = true;
                                    }
                                    Err(error) => {
                                        status_banner = format!("fork session failed: {error}");
                                    }
                                    }
                                }
                            }
                            it::session_meta::CrossProjectSessionAction::Cancel => {
                                startup_cross_project_cancelled = true;
                                status_banner = "Aborted.".to_string();
                                close_modal = true;
                            }
                            it::session_meta::CrossProjectSessionAction::None => {}
                        }
                    }
                    Modal::Trust(sel) => {
                        let action = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        match action {
                            it::selectors::TrustSelectorAction::Select(selection) => {
                                let trust_store =
                                    crate::core::project_trust::ProjectTrustStore::new(
                                        &runtime.extension_agent_dir,
                                    );
                                if selection.updates.is_empty() {
                                    // The current project selector exposes
                                    // durable choices, but preserve the
                                    // action contract if session-only options
                                    // are added later.
                                    settings.set_project_trusted(selection.trusted);
                                    status_banner = format!(
                                        "Project trust: {} for this session",
                                        if selection.trusted {
                                            "trusted"
                                        } else {
                                            "untrusted"
                                        }
                                    );
                                    close_modal = true;
                                } else {
                                    match trust_store.try_set_many(&selection.updates) {
                                        Ok(()) => {
                                            status_banner = format!(
                                                "Saved trust decision: {}. Restart pi for this to take effect.",
                                                if selection.trusted {
                                                    "trusted"
                                                } else {
                                                    "untrusted"
                                                }
                                            );
                                            close_modal = true;
                                        }
                                        Err(error) => {
                                            // Keep the selector open so a
                                            // readonly or malformed store can
                                            // be repaired/retried without
                                            // losing focus or pretending the
                                            // decision was durable.
                                            status_banner =
                                                format!("Could not save project trust: {error}");
                                        }
                                    }
                                }
                            }
                            it::selectors::TrustSelectorAction::Cancel => {
                                close_modal = true;
                            }
                            it::selectors::TrustSelectorAction::None => {}
                        }
                    }
                    Modal::Tree(sel) => {
                        let action = {
                            let mut guard = sel.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle(&key)
                        };
                        match action {
                            it::tree_selector::TreeSelectorAction::Select(entry_id) => {
                                close_modal = true;
                                let current_leaf = runtime.session.get_leaf_id().await.ok().flatten();
                                if current_leaf.as_deref() == Some(entry_id.as_str()) {
                                    status_banner = "Already at this point".to_string();
                                } else if streaming
                                    || llama_operation.is_some()
                                    || bash_operation.is_some()
                                {
                                    status_banner =
                                        "Wait for the current operation to finish before navigating the session tree."
                                            .to_string();
                                } else if !session_switch_allowed(&runtime, "navigate", None) {
                                    status_banner = "session navigation cancelled by extension".to_string();
                                } else {
                                    let target_entry = runtime.session.get_entry(&entry_id).await;
                                    match target_entry {
                                        None => {
                                            status_banner = format!("tree entry not found: {entry_id}");
                                        }
                                        Some(target_entry) => {
                                            // Selecting a user message positions the lane at its
                                            // parent so the next prompt can replace/re-edit it,
                                            // matching Pi's navigation semantics.
                                            let target_is_user = matches!(
                                                &target_entry,
                                                Entry::Message {
                                                    message: pi_agent::types::AgentMessage::Core(
                                                        Message::User(_),
                                                    ),
                                                    ..
                                                }
                                            );
                                            let editor_text = target_entry.as_message().and_then(|message| {
                                                match message {
                                                    pi_agent::types::AgentMessage::Core(Message::User(user)) => {
                                                        Some(pi_agent::agent::user_content_text(user))
                                                    }
                                                    _ => None,
                                                }
                                            });
                                            let new_leaf_id = if target_is_user {
                                                target_entry.parent_id().map(str::to_owned)
                                            } else {
                                                Some(entry_id.clone())
                                            };
                                            invalidate_interactive_harness(&mut runtime);
                                            match runtime
                                                .session
                                                .move_lane("main", new_leaf_id.as_deref())
                                                .await
                                            {
                                                Ok(()) => {
                                                    let (messages, cache_entries) =
                                                        rehydrate_transcript(&runtime, &transcript_md, hide_thinking)
                                                            .await;
                                                    runtime.messages = messages;
                                                    runtime.cache_entries = cache_entries;
                                                    runtime.persisted_until = runtime.messages.len();
                                                    status_log.clear();
                                                    clear_easter_egg_components(
                                                        &mut easter_egg_components,
                                                        &mut easter_egg_animation_until,
                                                    );
                                                    if let Some(text) = editor_text {
                                                        if editor.lock().unwrap_or_else(|error| error.into_inner()).get_text().trim().is_empty() {
                                                            editor.lock().unwrap_or_else(|error| error.into_inner()).set_text(&text);
                                                        }
                                                    }
                                                    status_banner = "Navigated to selected point".to_string();
                                                }
                                                Err(error) => {
                                                    status_banner = format!("navigate tree failed: {error}");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            it::tree_selector::TreeSelectorAction::Cancel => {
                                close_modal = true;
                            }
                            it::tree_selector::TreeSelectorAction::None => {}
                        }
                    }
                    Modal::Settings(panel) => {
                        let was_submenu_open = panel.lock().unwrap_or_else(|error| error.into_inner()).is_submenu_open();
                        {
                            let mut guard = panel.lock().unwrap_or_else(|error| error.into_inner());
                            guard.handle_input(&key);
                        }
                        let submenu_open = panel.lock().unwrap_or_else(|error| error.into_inner()).is_submenu_open();
                        let previews = { panel.lock().unwrap_or_else(|error| error.into_inner()).drain_previews() };
                        let changes = { panel.lock().unwrap_or_else(|error| error.into_inner()).drain_changes() };
                        let had_preview = !previews.is_empty();
                        for (id, value) in previews {
                            if id == "theme" {
                                match load_interactive_theme_checked(&value) {
                                    Ok(()) => {
                                        refresh_interactive_theme_views(
                                            &transcript_md,
                                            &transcript_view,
                                        );
                                    }
                                    Err(message) => {
                                        status_banner =
                                            format!("/settings theme preview failed: {message}");
                                    }
                                }
                            }
                        }
                        if had_preview {
                            renderer.invalidate();
                        }
                        if !changes.is_empty() {
                            for (id, value) in changes {
                                let mut error = None;
                                // Pi only shows a status line for settings
                                // whose upstream callback explicitly calls
                                // showStatus (currently HTTP timeout and TUI
                                // mode). Ordinary settings changes update the
                                // live UI without inventing a `/settings ...`
                                // transcript/status row.
                                let mut status = None;
                                match id.as_str() {
                                "autocompact" => {
                                    settings.set_compaction_enabled(value == "true");
                                    runtime.compaction_settings =
                                        interactive_compaction_settings(&settings);
                                    invalidate_interactive_harness(&mut runtime);
                                }
                                "show-images" | "images" => {
                                    settings.set_show_images(value == "true" || value == "on");
                                    renderer.invalidate();
                                }
                                "image-width-cells" => match value.parse::<f64>() {
                                    Ok(width) => settings.set_image_width_cells(width),
                                    Err(_) => {
                                        error = Some(format!("invalid image width: {value}"));
                                    }
                                },
                                "auto-resize-images" => {
                                    let enabled = value == "true";
                                    settings.set_image_auto_resize(enabled);
                                    runtime.auto_resize_images = enabled;
                                    invalidate_interactive_harness(&mut runtime);
                                }
                                "block-images" => {
                                    let blocked = value == "true";
                                    settings.set_block_images(blocked);
                                    runtime.block_images = blocked;
                                    invalidate_interactive_harness(&mut runtime);
                                }
                                "skill-commands" => {
                                    let enabled = value == "true";
                                    settings.set_enable_skill_commands(enabled);
                                    editor.lock().unwrap_or_else(|error| error.into_inner()).set_autocomplete_provider(Box::new(
                                        it::build_autocomplete_provider_with_skills(
                                            cwd.clone(),
                                            &runtime.skills,
                                            enabled,
                                        ),
                                    ));
                                }
                                "show-hardware-cursor" => {
                                    let enabled = value == "true";
                                    settings.set_show_hardware_cursor(enabled);
                                    renderer.set_show_hardware_cursor(enabled);
                                }
                                "editor-padding" => match value.parse::<f64>() {
                                    Ok(padding) => {
                                        settings.set_editor_padding_x(padding);
                                        editor
                                            .lock().unwrap_or_else(|error| error.into_inner())
                                            .set_padding_x(padding.max(0.0) as usize);
                                    }
                                    Err(_) => {
                                        error = Some(format!("invalid editor padding: {value}"));
                                    }
                                },
                                "output-padding" => match value.parse::<u64>() {
                                    Ok(padding) => {
                                        settings.set_output_pad(padding);
                                        transcript_view
                                            .lock().unwrap_or_else(|error| error.into_inner())
                                            .set_output_pad(padding as usize);
                                    }
                                    Err(_) => {
                                        error = Some(format!("invalid output padding: {value}"));
                                    }
                                },
                                "autocomplete-max-visible" => match value.parse::<f64>() {
                                    Ok(max_visible) => {
                                        settings.set_autocomplete_max_visible(max_visible);
                                        editor.lock().unwrap_or_else(|error| error.into_inner()).set_autocomplete_max_visible(
                                            max_visible.max(3.0) as usize,
                                        );
                                    }
                                    Err(_) => {
                                        error = Some(format!(
                                            "invalid autocomplete item count: {value}"
                                        ));
                                    }
                                },
                                "clear-on-shrink" => {
                                    let enabled = value == "true";
                                    settings.set_clear_on_shrink(enabled);
                                    renderer.set_clear_on_shrink(enabled);
                                }
                                "terminal-progress" => {
                                    let enabled = value == "true";
                                    settings.set_show_terminal_progress(enabled);
                                    if !enabled {
                                        renderer.terminal_handle().lock().unwrap_or_else(|error| error.into_inner()).set_progress(false);
                                    }
                                }
                                "steering-mode" => {
                                    settings.set_steering_mode(&value);
                                    if let Some(agent) = runtime
                                        .interactive_harness
                                        .as_ref()
                                        .and_then(|harness| harness.agent_handle())
                                    {
                                        apply_interactive_queue_mode(
                                            &agent,
                                            "steering-mode",
                                            &value,
                                        );
                                    }
                                }
                                "follow-up-mode" => {
                                    settings.set_follow_up_mode(&value);
                                    if let Some(agent) = runtime
                                        .interactive_harness
                                        .as_ref()
                                        .and_then(|harness| harness.agent_handle())
                                    {
                                        apply_interactive_queue_mode(
                                            &agent,
                                            "follow-up-mode",
                                            &value,
                                        );
                                    }
                                }
                                "transport" => {
                                    settings.set_transport(&value);
                                    runtime.transport = value;
                                    invalidate_interactive_harness(&mut runtime);
                                }
                                "http-idle-timeout" => {
                                    let timeout_ms = match value.as_str() {
                                        "30 sec" => Some(30_000),
                                        "1 min" => Some(60_000),
                                        "2 min" => Some(120_000),
                                        "5 min" => Some(300_000),
                                        "disabled" => Some(0),
                                        _ => None,
                                    };
                                    match timeout_ms {
                                        Some(timeout_ms) => {
                                            if let Err(message) = settings
                                                .set_http_idle_timeout_ms(timeout_ms as f64)
                                            {
                                                error = Some(message);
                                            } else {
                                                runtime.http_idle_timeout_ms = timeout_ms;
                                                invalidate_interactive_harness(&mut runtime);
                                                status = Some(format!(
                                                    "HTTP idle timeout: {value}"
                                                ));
                                            }
                                        }
                                        None => {
                                            error = Some(format!(
                                                "invalid HTTP idle timeout: {value}"
                                            ));
                                        }
                                    }
                                }
                                "hide-thinking" => {
                                    hide_thinking = value == "true";
                                    settings.set_hide_thinking_block(hide_thinking);
                                    transcript_view.lock().unwrap_or_else(|error| error.into_inner()).invalidate();
                                }
                                "mermaid-rendering" => {
                                    settings.set_mermaid_rendering_mode(&value);
                                    if let Ok(mut mode) = mermaid_mode.lock() {
                                        *mode = value.clone();
                                    }
                                    transcript_view.lock().unwrap_or_else(|error| error.into_inner()).invalidate();
                                }
                                "cache-miss-notices" => {
                                    settings.set_show_cache_miss_notices(value == "true");
                                }
                                "collapse-changelog" => {
                                    settings.set_collapse_changelog(value == "true");
                                }
                                "quiet-startup" => {
                                    settings.set_quiet_startup(value == "true");
                                }
                                "install-telemetry" => {
                                    settings.set_enable_install_telemetry(value == "true");
                                }
                                "default-project-trust" => {
                                    let trust = match value.as_str() {
                                        "Always trust" => "always",
                                        "Never trust" => "never",
                                        _ => "ask",
                                    };
                                    settings.set_default_project_trust(trust);
                                }
                                "double-escape-action" => {
                                    settings.set_double_escape_action(&value);
                                }
                                "tree-filter-mode" => {
                                    settings.set_tree_filter_mode(&value);
                                }
                                "tui-mode" => {
                                    match value.as_str() {
                                        "regular" | "fullscreen" => {
                                            let fullscreen = value == "fullscreen";
                                            if renderer.switch_mode(
                                                fullscreen,
                                                editor.clone(),
                                                &mut pending_loader.lock().unwrap_or_else(|error| error.into_inner()),
                                                settings.get_show_hardware_cursor(),
                                                settings.get_clear_on_shrink(),
                                            ) {
                                                use_alt_screen = fullscreen;
                                                transcript_scroll_view.lock().unwrap_or_else(|error| error.into_inner()).set_scrollbar(
                                                    fullscreen_scrollbar_mode(
                                                        settings.get_fullscreen_scrollbar(),
                                                        fullscreen,
                                                    ),
                                                );
                                                close_modal = true;
                                                settings.set_tui_mode(&value);
                                                status = Some(format!("TUI mode: {value}"));
                                            } else {
                                                panel.lock().unwrap_or_else(|error| error.into_inner()).update_value(
                                                    "tui-mode",
                                                    if use_alt_screen {
                                                        "fullscreen"
                                                    } else {
                                                        "regular"
                                                    },
                                                );
                                                error = Some(format!(
                                                    "unable to switch TUI mode to {value}"
                                                ));
                                            }
                                        }
                                        _ => {
                                            error = Some(format!("invalid TUI mode: {value}"));
                                        }
                                    }
                                }
                                "fullscreen-exit-output" => {
                                    settings.set_fullscreen_exit_output(&value);
                                }
                                "fullscreen-scrollbar" => {
                                    settings.set_fullscreen_scrollbar(&value);
                                    transcript_scroll_view.lock().unwrap_or_else(|error| error.into_inner()).set_scrollbar(
                                        fullscreen_scrollbar_mode(&value, use_alt_screen),
                                    );
                                }
                                "thinking" => {
                                    invalidate_interactive_harness(&mut runtime);
                                    settings.set_default_thinking_level(&value);
                                    thinking_level = value.clone();
                                    _session_environment.set_reasoning_level(&thinking_level);
                                }
                                "theme" => {
                                    match load_interactive_theme_setting_checked(&value) {
                                        Ok(()) => {
                                            settings.set_theme(value);
                                            refresh_interactive_theme_views(
                                                &transcript_md,
                                                &transcript_view,
                                            );
                                        }
                                        Err(message) => {
                                            error = Some(message);
                                        }
                                    }
                                }
                                "warnings" => {
                                    match value.split_once('=') {
                                        Some((warning_id, enabled))
                                            if warning_id == "anthropic-extra-usage" =>
                                        {
                                            match enabled.parse::<bool>() {
                                                Ok(enabled) => {
                                                    let mut warnings = settings.get_warnings();
                                                    warnings.insert(
                                                        warning_id.to_string(),
                                                        Value::Bool(enabled),
                                                    );
                                                    settings.set_warnings(warnings);
                                                }
                                                Err(_) => {
                                                    error = Some(format!(
                                                        "invalid warning value: {enabled}"
                                                    ));
                                                }
                                            }
                                        }
                                        Some((warning_id, _)) => {
                                            error = Some(format!("unknown warning: {warning_id}"));
                                        }
                                        None => {
                                            error = Some(format!(
                                                "invalid warning setting: {value}"
                                            ));
                                        }
                                    }
                                }
                                "model-thinking" => {
                                    match value.split_once('=') {
                                        Some((model_ref, level)) => {
                                            match model_ref.split_once('/') {
                                                Some((provider, model_id)) => {
                                                    let is_current = provider == runtime.provider
                                                        && model_id == runtime.model.id;
                                                    if level == "__clear__" {
                                                        settings.remove_model_thinking_level(
                                                            provider, model_id,
                                                        );
                                                        if is_current {
                                                            let next_level = settings
                                                                .get_default_thinking_level()
                                                                .unwrap_or(
                                                                    crate::core::model_resolver::DEFAULT_THINKING_LEVEL,
                                                                )
                                                                .to_string();
                                                            thinking_level = next_level;
                                                            hide_thinking = thinking_level == "off"
                                                                || settings.get_hide_thinking_block();
                                                            _session_environment
                                                                .set_reasoning_level(&thinking_level);
                                                        }
                                                    } else if it::selectors::THINKING_LEVELS
                                                        .contains(&level)
                                                    {
                                                        settings.set_model_thinking_level(
                                                            provider, model_id, level,
                                                        );
                                                        if is_current {
                                                            thinking_level = level.to_string();
                                                            hide_thinking = thinking_level == "off"
                                                                || settings.get_hide_thinking_block();
                                                            _session_environment
                                                                .set_reasoning_level(&thinking_level);
                                                        }
                                                    } else {
                                                        error = Some(format!(
                                                            "invalid thinking level: {level}"
                                                        ));
                                                    }
                                                    if error.is_none() {
                                                        let configured = settings
                                                            .get_all_model_thinking_levels()
                                                            .len();
                                                        panel.lock().unwrap_or_else(|error| error.into_inner()).update_submenu_display_value(
                                                            "model-thinking",
                                                            if configured == 0 {
                                                                "none".to_string()
                                                            } else {
                                                                format!("{configured} configured")
                                                            },
                                                        );
                                                        invalidate_interactive_harness(&mut runtime);
                                                    }
                                                }
                                                None => {
                                                    error = Some(format!(
                                                        "invalid model reference: {model_ref}"
                                                    ));
                                                }
                                            }
                                        }
                                        None => {
                                            error = Some(format!(
                                                "invalid model-thinking setting: {value}"
                                            ));
                                        }
                                    }
                                }
                                _ => {
                                    status = Some(format!("unsupported setting {id}"));
                                }
                            }
                            // SettingsManager setters update memory immediately but enqueue
                            // disk writes. Interactive settings must be durable before the
                            // modal reports success, matching Pi's settings callback contract
                            // and preventing a clean quit from losing the change. Surface a
                            // failed write instead of reporting the in-memory value as saved.
                            settings.flush().await;
                            if error.is_none() {
                                if let Some(write_error) =
                                    settings.drain_errors().into_iter().next()
                                {
                                    error = Some(write_error.error);
                                }
                            } else {
                                let _ = settings.drain_errors();
                            }
                            if let Some(message) = error {
                                status_banner = format!("/settings {id} failed: {message}");
                            } else if let Some(status) = status {
                                status_banner = status;
                            }
                            renderer.invalidate();
                        }
                        }
                        if (key.base == "esc" || key.base == "escape")
                            && !was_submenu_open
                            && !submenu_open
                        {
                            close_modal = true;
                        }
                    }
                }
                if close_modal {
                    modal = None;
                    renderer.focus(editor.clone());
                }
                // Modal components retain their rendered lines.  Input that
                // only changes a search query or selection does not emit a
                // settings preview/change callback, so invalidate at the
                // modal boundary after every dispatch.  This keeps ordinary
                // modal typing/navigation visible and also refreshes the
                // underlying scene when the modal closes.
                renderer.invalidate();
                if startup_resume_cancelled || startup_cross_project_cancelled {
                    return Ok(());
                }
                continue;
            }

            // App actions which temporarily take ownership of the terminal
            // must stop the long-lived stdin worker. Otherwise it can consume
            // editor input while an external process or clipboard probe owns
            // the terminal.
            let external_editor_key = !streaming && key.ctrl && key.base == "g";
            let clipboard_key = !streaming
                && ((key.ctrl && key.base == "v")
                    || (key.alt && key.base == "v" && cfg!(windows)));
            if external_editor_key {
                let content = editor.lock().unwrap_or_else(|error| error.into_inner()).get_expanded_text();
                input.stop_worker().await;
                let terminal_handle = renderer.terminal_handle();
                let leave_error = terminal_handle.lock().unwrap_or_else(|error| error.into_inner()).leave_raw().err();
                let result = if let Some(error) = leave_error {
                    it::external_editor::ExternalEditorResult::Failed(format!(
                        "restore terminal before external editor: {error}"
                    ))
                } else {
                    it::external_editor::edit_in_external_editor(
                        it::external_editor::ExternalEditorOptions {
                            command: settings.get_external_editor_command(),
                            content,
                        },
                    )
                    .await
                };
                let reenter = terminal_handle
                    .lock().unwrap_or_else(|error| error.into_inner())
                    .enter_raw_with_alt_screen(use_alt_screen);
                input.restart().await;
                match reenter {
                    Err(error) => status_banner = format!("terminal restore failed: {error}"),
                    Ok(()) => match result {
                        it::external_editor::ExternalEditorResult::Complete(content) => {
                            editor.lock().unwrap_or_else(|error| error.into_inner()).set_text(&content);
                            status_banner = "external editor complete".to_string();
                        }
                        it::external_editor::ExternalEditorResult::Cancelled => {
                            status_banner = "external editor cancelled".to_string();
                        }
                        it::external_editor::ExternalEditorResult::Failed(error) => {
                            status_banner = format!("external editor failed: {error}");
                        }
                    },
                }
                renderer.invalidate();
                continue;
            }
            if clipboard_key {
                input.stop_worker().await;
                let image = it::clipboard::read_clipboard_image().await;
                let text = if image.is_none() {
                    it::clipboard::read_clipboard_text().await
                } else {
                    None
                };
                input.restart().await;
                match image {
                    Some(image) => match it::clipboard::write_image_attachment(&image) {
                        Ok(path) => {
                            editor
                                .lock().unwrap_or_else(|error| error.into_inner())
                                .insert_text_at_cursor(&path.to_string_lossy());
                            status_banner = format!("pasted {} image", image.mime_type);
                        }
                        Err(error) => status_banner = error.0,
                    },
                    None => match text {
                        Some(text) => {
                            editor.lock().unwrap_or_else(|error| error.into_inner()).insert_text_at_cursor(&text);
                            status_banner = "pasted clipboard text".to_string();
                        }
                        None => status_banner =
                            "clipboard unavailable (no readable text or image backend)".to_string(),
                    },
                }
                renderer.invalidate();
                continue;
            }

            // Editor input (skip Enter/Ctrl+C which the parent handles).
            if key.ctrl && key.base == "c" {
                continue;
            }
            apply_interactive_editor_input(&editor, &key_str, &key);
            let bash_mode = editor.lock().unwrap_or_else(|error| error.into_inner()).starts_with_non_whitespace('!');
            sync_editor_border(
                &editor,
                &thinking_level,
                bash_mode,
                &mut last_editor_border_state,
            );

            // Pi renders focused editor input immediately.  The cached scene
            // contains the same shared Editor handle, so this repaint exposes
            // the new draft before the next iteration performs the more
            // expensive transcript/footer/scene preparation pass.
            let immediately_repainted = renderer.render_cached_scene(cached_scene.as_ref());

            // Submit?
            let submitted = editor.lock().unwrap_or_else(|error| error.into_inner()).drain_submitted();
            let had_submission = submitted.is_some();
            if let Some(submitted) = submitted {
                if submitted.trim().is_empty() || streaming {
                    continue;
                }
                if let Some(command_result) =
                    execute_interactive_extension_command(&runtime, &submitted)
                {
                    status_banner = command_result;
                    let lifecycle_notes = apply_pending_extension_lifecycle_actions(
                        &mut runtime,
                        &mut settings,
                        &thinking_level,
                        &transcript_md,
                        hide_thinking,
                    )
                    .await;
                    refresh_startup_presentation(
                        startup_presentation.as_ref(),
                        &runtime,
                        args,
                        &settings,
                    );
                    if !lifecycle_notes.is_empty() {
                        status_banner.push_str("; ");
                        status_banner.push_str(&lifecycle_notes.join("; "));
                    }
                    continue;
                }
                if let Some((command, exclude_from_context)) = parse_bash_submission(&submitted) {
                    if command.trim().is_empty() {
                        status_banner = if exclude_from_context {
                            "usage: !!<command>".to_string()
                        } else {
                            "usage: !<command> or !!<command>".to_string()
                        };
                        continue;
                    }
                    editor.lock().unwrap_or_else(|error| error.into_inner()).add_to_history(&submitted);
                    streaming = true;
                    pending_text.clear();
                    status_banner.clear();
                    bash_operation = Some(start_interactive_bash_operation(
                        command,
                        cwd.clone(),
                        exclude_from_context,
                        Arc::clone(&stream_buffer),
                        runtime.shell_command_prefix.clone(),
                        runtime.shell_path.clone(),
                    ));
                    continue;
                }
                let action = it::parse_submit(&submitted);
                match action {
                    SubmitAction::Prompt(prompt) => {
                        let expanded_prompt = it::expand_skill_command(&prompt, &runtime.skills);
                        let mut pending_turns = VecDeque::from([InteractivePendingMessage {
                            text: crate::core::prompt_templates::expand_prompt_template(
                                &expanded_prompt,
                                &runtime.prompt_templates,
                            ),
                            kind: InteractiveQueueKind::Steering,
                        }]);
                        while let Some(next_turn) = pending_turns.pop_front() {
                            editor.lock().unwrap_or_else(|error| error.into_inner()).add_to_history(&next_turn.text);
                            streaming = true;
                            let terminal_progress_enabled = settings.get_show_terminal_progress();
                            if terminal_progress_enabled {
                                renderer.terminal_handle().lock().unwrap_or_else(|error| error.into_inner()).set_progress(true);
                            }
                            pending_text = interactive_working_message(next_turn.kind).to_string();
                            let render_options = it::messages::TranscriptRenderOptions {
                                hide_thinking,
                                show_images: settings.get_show_images(),
                                image_width_cells: settings.get_image_width_cells() as usize,
                                output_pad: settings.get_output_pad() as usize,
                                expand_tool_output: tool_output_expanded.load(Ordering::Acquire),
                            };
                            {
                                let mut live = live_transcript.lock().unwrap_or_else(|error| error.into_inner());
                                live.clear();
                                live.configure(render_options);
                            }
                            *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = String::new();
                            let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = {
                                let live_transcript = live_transcript.clone();
                                let stream_buffer = stream_buffer.clone();
                                let tool_output_expanded = tool_output_expanded.clone();
                                Arc::new(move |event: &AssistantMessageEvent| {
                                    let mut options = render_options;
                                    options.expand_tool_output =
                                        tool_output_expanded.load(Ordering::Acquire);
                                    let rendered = it::messages::render_assistant_event_without_tool_calls_with_options(
                                        event, options,
                                    );
                                    let mut live = live_transcript.lock().unwrap_or_else(|error| error.into_inner());
                                    live.configure(options);
                                    live.on_assistant_event(event, rendered);
                                    let rendered = live.render();
                                    drop(live);
                                    *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = rendered;
                                })
                            };
                            let on_tool_event: Arc<dyn Fn(&RichAgentEvent) + Send + Sync> = {
                                let live_transcript = live_transcript.clone();
                                let stream_buffer = stream_buffer.clone();
                                let tool_output_expanded = tool_output_expanded.clone();
                                Arc::new(move |event: &RichAgentEvent| {
                                    let mut live = live_transcript.lock().unwrap_or_else(|error| error.into_inner());
                                    let mut options = render_options;
                                    options.expand_tool_output =
                                        tool_output_expanded.load(Ordering::Acquire);
                                    live.configure(options);
                                    live.on_tool_event(event);
                                    let rendered = live.render();
                                    drop(live);
                                    *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = rendered;
                                })
                            };
                            let (turn_result, newly_queued) = {
                                let mut ui = InteractiveStreamingUi {
                                    renderer: &mut renderer,
                                    editor: &editor,
                                    transcript_md: &transcript_md,
                                    transcript_view: &transcript_view,
                                    transcript_scroll_view: &transcript_scroll_view,
                                    status_text: &status_text,
                                    status_log: &status_log,
                                    footer_text: &footer_text,
                                    pending_loader: &pending_loader,
                                    stream_buffer: &stream_buffer,
                                    pending_text: &mut pending_text,
                                    live_transcript: &live_transcript,
                                    status_banner: &mut status_banner,
                                    hide_thinking,
                                    show_images: settings.get_show_images(),
                                    image_width_cells: settings.get_image_width_cells() as usize,
                                    output_pad: settings.get_output_pad() as usize,
                                    tool_output_expanded: tool_output_expanded.as_ref(),
                                    last_projection_key: None,
                                    cached_scene_pending: None,
                                    cached_scene: None,
                                };
                                stream_turn_with_input(
                                    &mut runtime,
                                    next_turn.text,
                                    on_event,
                                    on_tool_event,
                                    &mut ui,
                                    InteractiveTurnInput {
                                        input: &mut input,
                                        steering_mode: settings.get_steering_mode(),
                                        follow_up_mode: settings.get_follow_up_mode(),
                                        session_environment: Some(&_session_environment),
                                        #[cfg(unix)]
                                        sigcont: &mut sigcont,
                                        #[cfg(unix)]
                                        use_alt_screen,
                                    },
                                )
                                .await
                            };
                            if terminal_progress_enabled {
                                renderer.terminal_handle().lock().unwrap_or_else(|error| error.into_inner()).set_progress(false);
                            }
                            if let Some(error) = interactive_turn_error_banner(&turn_result) {
                                status_banner = error;
                            }
                            // `finish_interactive_turn` may replace the active
                            // context after compaction, so the pre-turn
                            // message length is not a valid slice boundary.
                            // The harness already returns the exact message
                            // delta for cache/accounting purposes.
                            let new_messages = turn_result.clone().unwrap_or_default();
                            append_cache_entries_from_messages(&mut runtime.cache_entries, &new_messages);
                            streaming = false;
                            pending_text = String::new();
                            live_transcript.lock().unwrap_or_else(|error| error.into_inner()).clear();
                            *stream_buffer.lock().unwrap_or_else(|error| error.into_inner()) = String::new();
                            pending_turns.extend(newly_queued);
                            let lifecycle_notes = apply_pending_extension_lifecycle_actions(
                                &mut runtime,
                                &mut settings,
                                &thinking_level,
                                &transcript_md,
                                hide_thinking,
                            )
                            .await;
                            refresh_startup_presentation(
                                startup_presentation.as_ref(),
                                &runtime,
                                args,
                                &settings,
                            );
                            if !lifecycle_notes.is_empty() {
                                status_banner = lifecycle_notes.join("; ");
                            }
                            // Auto-compaction: summarize history when the context
                            // approaches the model window (upstream compaction loop).
                            let terminal_progress_enabled = settings.get_show_terminal_progress();
                            if terminal_progress_enabled {
                                renderer.terminal_handle().lock().unwrap_or_else(|error| error.into_inner()).set_progress(true);
                            }
                            match maybe_auto_compact(&mut runtime, &settings).await {
                                Ok(true) => status_banner = "context compacted (auto)".to_string(),
                                Ok(false) => {}
                                Err(e) => status_banner = e,
                            }
                            if terminal_progress_enabled {
                                renderer.terminal_handle().lock().unwrap_or_else(|error| error.into_inner()).set_progress(false);
                            }
                        }
                    }
                    SubmitAction::Command(command, arg) => {
                        match command.kind {
                            SlashKind::Model => {
                                if let Some(value) = arg.as_deref() {
                                    match apply_model_reference(&mut runtime, value) {
                                        Ok(message) => {
                                            _session_environment
                                                .set_model(&runtime.provider, &runtime.model.name);
                                            status_banner = message;
                                            if maybe_add_daxnuts_component(
                                                &runtime,
                                                &mut easter_egg_components,
                                                &mut easter_egg_animation_until,
                                            ) {
                                                hidden_component_boundary_pending = true;
                                            }
                                        }
                                        Err(error) => status_banner = error,
                                    }
                                } else {
                                    let model_reload_notes =
                                        reload_interactive_models(&mut runtime);
                                    if !model_reload_notes.is_empty() {
                                        status_banner = model_reload_notes.join("; ");
                                    }
                                    let current_model =
                                        Some(format!("{}/{}", runtime.provider, runtime.model.id));
                                    let default_model = settings
                                        .get_default_provider()
                                        .zip(settings.get_default_model())
                                        .map(|(provider, model)| format!("{provider}/{model}"));
                                    let selector = Arc::new(Mutex::new(
                                        it::selectors::ModelSelector::new_with_scoped_models(
                                            runtime.models.get_available(None),
                                            &runtime.scoped_models,
                                            current_model,
                                            default_model,
                                        ),
                                    ));
                                    let refresh_selector = Arc::clone(&selector);
                                    let refresh_models = runtime.models.clone();
                                    let allow_network =
                                        !args.offline && !config::env_flag(config::ENV_OFFLINE);
                                    tokio::spawn(async move {
                                        let result = refresh_models
                                            .refresh(pi_ai::models::ModelsRefreshOptions {
                                                allow_network,
                                                providers: None,
                                                force: false,
                                                signal: None,
                                            })
                                            .await;
                                        let available = refresh_models.get_available(None);
                                        if let Ok(mut guard) = refresh_selector.lock() {
                                            guard.apply_refresh(available, &result);
                                        }
                                    });
                                    modal = Some(Modal::Model(selector));
                                }
                            }
                            SlashKind::Llama => {
                                it::llama::register_provider(&runtime.models);
                                let argument = arg.as_deref().map(str::trim).filter(|value| !value.is_empty());
                                match argument {
                                    Some(value) if value.eq_ignore_ascii_case("refresh") => {
                                        match it::llama::LlamaManager::open(&runtime.models).await {
                                            Ok(_) => status_banner = "llama.cpp catalog refreshed".to_string(),
                                            Err(error) => status_banner = error,
                                        }
                                    }
                                    Some(value) if value.to_ascii_lowercase().starts_with("search ") => {
                                        let query = value[7..].trim();
                                        if query.is_empty() {
                                            status_banner = "usage: /llama search <Hugging Face query>".to_string();
                                        } else {
                                            match start_huggingface_search_operation(query.to_owned()) {
                                                Ok(operation) => {
                                                    llama_operation = Some(operation);
                                                    streaming = true;
                                                    status_banner =
                                                        "searching Hugging Face…".to_string();
                                                }
                                                Err(error) => status_banner = error,
                                            }
                                        }
                                    }
                                    Some(value) if value.to_ascii_lowercase().starts_with("download ") => {
                                        let spec = value[9..].trim();
                                        match it::llama::client_for_models(&runtime.models) {
                                            Ok(client) => {
                                                llama_operation = Some(start_llama_download_operation(
                                                    client,
                                                    spec.to_owned(),
                                                ));
                                                streaming = true;
                                                status_banner = "llama.cpp download started".to_string();
                                            }
                                            Err(error) => status_banner = error,
                                        }
                                    }
                                    Some(_) => {
                                        status_banner = "usage: /llama, /llama refresh, /llama search <query>, or /llama download <repo>:<quantization>".to_string();
                                    }
                                    None => {
                                        match it::llama::LlamaManager::open(&runtime.models).await {
                                            Ok(manager) => {
                                                modal = Some(Modal::Llama(Arc::new(Mutex::new(
                                                    it::llama::LlamaSelector::new(manager.options()),
                                                ))));
                                                status_banner = format!(
                                                    "llama.cpp connected to {}; loaded: {}",
                                                    manager.client.server_url(),
                                                    it::llama::loaded_model_ids(&manager.catalog).join(", ")
                                                );
                                            }
                                            Err(error) => status_banner = error,
                                        }
                                    }
                                }
                            }
                            SlashKind::ScopedModels => {
                                let items = it::selectors::model_selector_items(&runtime.models, None);
                                modal = Some(Modal::ScopedModels(Arc::new(Mutex::new(
                                    it::selectors::ScopedModelsSelector::new(
                                        items,
                                        &runtime.scoped_models,
                                    ),
                                ))));
                            }
                            SlashKind::Thinking => {
                                let items = it::selectors::thinking_selector_items_for_model(
                                    &pi_ai::model::get_supported_thinking_levels(&runtime.model),
                                );
                                modal = Some(Modal::Thinking(Arc::new(Mutex::new(
                                    it::selectors::ThinkingSelector::new(
                                        items,
                                        &thinking_level,
                                        settings.get_default_thinking_level(),
                                    ),
                                ))));
                            }
                            SlashKind::Theme => {
                                let items = it::selectors::theme_selector_items();
                                modal = Some(Modal::Theme(Arc::new(Mutex::new(ListSelector::new(items, 10)))));
                            }
                            SlashKind::Settings => {
                                let mut entries = it::selectors::settings_selector_items_for_runtime(
                                    &settings,
                                    &runtime.models,
                                    &runtime.provider,
                                    &runtime.model,
                                );
                                // CLI `--tui-mode` is invocation-local and may
                                // override the persisted setting. The live
                                // selector must describe the renderer that is
                                // actually active so Enter cycles from the
                                // correct state.
                                if let Some(entry) =
                                    entries.iter_mut().find(|entry| entry.id == "tui-mode")
                                {
                                    entry.current_value = if use_alt_screen {
                                        "fullscreen".to_string()
                                    } else {
                                        "regular".to_string()
                                    };
                                }
                                modal = Some(Modal::Settings(Arc::new(Mutex::new(SettingsPanel::new(entries)))));
                            }
                            SlashKind::Session => {
                                status_banner = session_status(&runtime);
                            }
                            SlashKind::Changelog => {
                                status_banner = changelog_status();
                            }
                            SlashKind::Clear => {
                                invalidate_interactive_harness(&mut runtime);
                                runtime.messages.clear();
                                status_log.clear();
                                clear_easter_egg_components(
                                    &mut easter_egg_components,
                                    &mut easter_egg_animation_until,
                                );
                                // `/clear` starts a fresh prompt-cache segment
                                // while retaining the session's historical
                                // accounting.
                                runtime.cache_entries.push(json!({
                                    "type": "compaction",
                                    "timestamp": pi_ai::types::now_ms(),
                                }));
                                transcript_md.lock().unwrap_or_else(|error| error.into_inner()).set_text("");
                            }
                            SlashKind::Hotkeys => {
                                status_banner = "hotkeys: enter submit · shift+enter newline · ctrl+c quit · ↑/↓ history · ctrl+w word-delete".to_string();
                            }
                            SlashKind::Help => {
                                status_banner = it::slash::help_banner();
                            }
                            SlashKind::Quit => {
                                return Ok(());
                            }
                            SlashKind::Login => {
                                let provider_ref = arg
                                    .as_deref()
                                    .map(|s| s.trim())
                                    .filter(|s| !s.is_empty());
                                let banner = Arc::new(Mutex::new(String::new()));
                                let surface = Arc::new(Mutex::new(AuthSurfaceState::dialog("Login")));
                                surface.lock().unwrap_or_else(|error| error.into_inner()).set_context_row(
                                    "→ login       <provider> — Configure provider authentication",
                                );
                                let term = renderer.terminal_handle();
                                input.stop_worker().await;
                                invalidate_interactive_harness(&mut runtime);
                                let auth_result = if input.pending_cancel() {
                                    Err("Login cancelled".to_string())
                                } else {
                                    run_login(
                                        &runtime.models,
                                        provider_ref,
                                        banner,
                                        term,
                                        surface,
                                    )
                                    .await
                                };
                                match auth_result {
                                    Ok(message) => status_banner = message,
                                    Err(error) => status_banner = error,
                                }
                                footer_invalidation_generation =
                                    footer_invalidation_generation.wrapping_add(1);
                                input.restart().await;
                                renderer.invalidate();
                            }
                            SlashKind::Logout => {
                                let provider_ref = arg
                                    .as_deref()
                                    .map(|s| s.trim())
                                    .filter(|s| !s.is_empty());
                                let banner = Arc::new(Mutex::new(String::new()));
                                let surface = Arc::new(Mutex::new(AuthSurfaceState::dialog("Logout")));
                                let term = renderer.terminal_handle();
                                input.stop_worker().await;
                                invalidate_interactive_harness(&mut runtime);
                                let logout_result = if input.pending_cancel() {
                                    Err("Logout cancelled".to_string())
                                } else {
                                    run_oauth_logout(
                                        &runtime.models,
                                        provider_ref,
                                        banner,
                                        term,
                                        surface,
                                    )
                                    .await
                                };
                                match logout_result {
                                    Ok(message) => status_banner = message,
                                    Err(error) => status_banner = format!("logout failed: {error}"),
                                }
                                footer_invalidation_generation =
                                    footer_invalidation_generation.wrapping_add(1);
                                input.restart().await;
                                renderer.invalidate();
                            }
                            SlashKind::Compact => {
                                let instructions = arg
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty());
                                match compact_interactive(
                                    &mut runtime,
                                    &settings,
                                    instructions,
                                    true,
                                )
                                .await
                                {
                                    Ok(true) => {
                                        invalidate_interactive_harness(&mut runtime);
                                        status_banner = "context compacted".to_string();
                                    }
                                    Ok(false) => status_banner = "nothing to compact".to_string(),
                                    Err(error) => status_banner = error,
                                }
                            }
                            SlashKind::Debug => {
                                let terminal_handle = renderer.terminal_handle();
                                let terminal = terminal_handle.lock().unwrap_or_else(|error| error.into_inner());
                                let width = terminal.width();
                                let height = terminal.height();
                                drop(terminal);
                                let lines = debug_render_lines(
                                    &transcript_md,
                                    &editor,
                                    &footer_text,
                                    &easter_egg_components,
                                    width,
                                );
                                let path = std::path::Path::new(&runtime.extension_agent_dir)
                                    .join(format!("{}-debug.log", config::APP_NAME));
                                match write_debug_snapshot(
                                    &path,
                                    width,
                                    height,
                                    &lines,
                                    &runtime.messages,
                                ) {
                                    Ok(()) => {
                                        status_banner =
                                            format!("✓ Debug log written\n{}", path.display());
                                    }
                                    Err(error) => {
                                        status_banner = format!("debug failed: {error}");
                                    }
                                }
                            }
                            SlashKind::ArminSaysHi => {
                                easter_egg_components.push(it::easter_eggs::armin_component());
                                hidden_component_boundary_pending = true;
                                easter_egg_animation_until = Some(
                                    std::time::Instant::now()
                                        + it::easter_eggs::animation_duration(),
                                );
                            }
                            SlashKind::DementedDelves => {
                                easter_egg_components.push(it::easter_eggs::earendil_component());
                                hidden_component_boundary_pending = true;
                            }
                            SlashKind::Export => {
                                    if !runtime.session_persistence {
                                        status_banner =
                                            "export requires a persistent session; remove --no-session"
                                                .to_string();
                                    } else {
                                        let meta = runtime.session.get_metadata().await;
                                        match crate::core::export_html::export_session_file(
                                            &meta.path,
                                            arg.as_deref(),
                                            None,
                                        ) {
                                            Ok(path) => {
                                                status_banner = format!("exported session to {path}");
                                            }
                                            Err(e) => {
                                                status_banner = format!("export failed: {e}");
                                            }
                                        }
                                    }
                                }
                                SlashKind::New => {
                                    if !session_switch_allowed(&runtime, "new", None) {
                                        status_banner = "new session cancelled by extension".to_string();
                                    } else {
                                        let previous_session_file =
                                            runtime.session.get_metadata().await.path;
                                        let new_id = pi_agent::session::new_id();
                                        let new_session = if runtime.session_persistence {
                                            runtime
                                                .repo
                                                .create(CreateOptions {
                                                    id: Some(new_id.clone()),
                                                    cwd: runtime.cwd.clone(),
                                                    parent_session_id: None,
                                                    metadata: None,
                                                    fork_options: ForkOptions::Tree,
                                                })
                                                .await
                                        } else {
                                            let mut metadata = in_memory_metadata(new_id.clone(), None);
                                            metadata.cwd = runtime.cwd.clone();
                                            Ok(JsonlSession::from_in_memory(Arc::new(Mutex::new(
                                                InMemorySessionStorage::new(metadata),
                                            ))))
                                        };
                                        match new_session {
                                        Ok(new_session) => {
                                            let target_session_file =
                                                new_session.get_metadata().await.path;
                                            let previous_target = runtime
                                                .session_persistence
                                                .then_some(previous_session_file.as_str());
                                            let target = runtime
                                                .session_persistence
                                                .then_some(target_session_file.as_str());
                                            shutdown_extensions_before_session_replace(
                                                &runtime,
                                                "new",
                                                target,
                                            );
                                            invalidate_interactive_harness(&mut runtime);
                                            runtime.session = new_session;
                                            runtime.session_id = new_id;
                                            runtime.session_name = None;
                                            runtime.messages.clear();
                                            runtime.cache_entries.clear();
                                            runtime.persisted_until = 0;
                                            status_log.clear();
                                            clear_easter_egg_components(
                                                &mut easter_egg_components,
                                                &mut easter_egg_animation_until,
                                            );
                                            transcript_md.lock().unwrap_or_else(|error| error.into_inner()).set_text("");
                                            let notes = replace_extensions(
                                                &mut runtime,
                                                &settings,
                                                &thinking_level,
                                                "new",
                                                previous_target,
                                                target,
                                            );
                                            refresh_startup_presentation(
                                                startup_presentation.as_ref(),
                                                &runtime,
                                                args,
                                                &settings,
                                            );
                                            status_banner = format!(
                                                "started new session {} in {}",
                                                runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                                                meta_short_cwd(&runtime.cwd)
                                            );
                                            if !notes.is_empty() {
                                                status_banner.push_str(&format!(
                                                    " (extensions: {})",
                                                    notes.join("; ")
                                                ));
                                            }
                                        }
                                            Err(e) => {
                                                status_banner = format!("new session failed: {e}");
                                            }
                                        }
                                    }
                                }
                                SlashKind::Resume => {
                                    if !runtime.session_persistence {
                                        status_banner =
                                            "resume requires a persistent session; remove --no-session"
                                                .to_string();
                                    } else {
                                        match crate::core::session_migration::migrate_legacy_sessions_in_root(
                                            std::path::Path::new(&runtime.session_root),
                                        ) {
                                            Ok(_) => match runtime.repo.list(None).await {
                                                Ok(sessions) => {
                                                    // Keep the active file out of the candidate list,
                                                    // but retain all projects so Tab can switch from
                                                    // the current-folder scope to the global scope.
                                                    let current_session_path =
                                                        runtime.session.get_metadata().await.path;
                                                    let sessions = resumable_sessions(
                                                        sessions,
                                                        &runtime.session_id,
                                                        Some(&current_session_path),
                                                    );
                                                    if sessions.is_empty() {
                                                        status_banner =
                                                            "no sessions found to resume in this directory"
                                                                .to_string();
                                                    } else {
                                                        let picker = it::session_picker_items(sessions);
                                                        let picker_state =
                                                            it::session_meta::SessionPickerState::new(
                                                                it::session_meta::session_picker_records(
                                                                    &picker,
                                                                ),
                                                                runtime.cwd.clone(),
                                                                Some(current_session_path),
                                                            );
                                                        modal = Some(Modal::Resume(
                                                            Arc::new(Mutex::new(picker_state)),
                                                            picker,
                                                        ));
                                                    }
                                                }
                                                Err(e) => {
                                                    status_banner = format!("list sessions failed: {e}");
                                                }
                                            },
                                            Err(e) => {
                                                status_banner = format!("migrate legacy sessions failed: {e}");
                                            }
                                        }
                                    }
                                }
                                SlashKind::Name => {
                                    match arg.as_deref() {
                                        Some(name) => {
                                            let normalized_name =
                                                crate::run::normalize_session_name_value(name);
                                            if normalized_name.is_empty() {
                                                status_banner =
                                                    "usage: /name <session-name>".to_string();
                                            } else {
                                                match runtime
                                                    .session
                                                    .set_name(Some(&normalized_name))
                                                    .await
                                                {
                                                    Ok(()) => {
                                                        let changed = normalized_name != name.trim();
                                                        runtime.session_name =
                                                            Some(normalized_name.clone());
                                                        status_banner = if changed {
                                                            format!(
                                                                "session name normalized to: {normalized_name}"
                                                            )
                                                        } else {
                                                            format!("session name: {normalized_name}")
                                                        };
                                                    }
                                                    Err(e) => {
                                                        status_banner =
                                                            format!("set name failed: {e}");
                                                    }
                                                }
                                            }
                                        }
                                        _ => {
                                            status_banner = "usage: /name <session-name>".to_string();
                                        }
                                    }
                                }
                                SlashKind::Import => {
                                    if !runtime.session_persistence {
                                        status_banner =
                                            "import requires a persistent session; remove --no-session"
                                                .to_string();
                                    } else {
                                    match arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                        None => {
                                            status_banner = "usage: /import <session.jsonl>".to_string();
                                        }
                                        Some(path) => {
                                            let input_path = config::expand_tilde_path(path);
                                            if !std::path::Path::new(&input_path).exists() {
                                                status_banner = format!("file not found: {path}");
                                            } else if let Ok(content) = std::fs::read_to_string(&input_path) {
                                                let header_id = content.lines().next().and_then(|line| {
                                                    serde_json::from_str::<serde_json::Value>(line)
                                                        .ok()
                                                        .and_then(|v| {
                                                            v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                                                        })
                                                });
                                                match header_id {
                                                    None => {
                                                        status_banner = format!("invalid session file: {path}");
                                                    }
                                                    Some(header_id) => {
                                                        // Legacy (v1-v3) files are converted to the v4
                                                        // harness JSONL format and written into the
                                                        // session dir before opening (upstream
                                                        // session-manager migration path).
                                                        let first_line = content.lines().next().unwrap_or("");
                                                        let is_v4 = serde_json::from_str::<serde_json::Value>(first_line)
                                                            .ok()
                                                            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(|s| s.to_string()))
                                                            == Some("header".to_string());
                                                        let resolved_path: Option<String> = if is_v4 {
                                                            Some(input_path.clone())
                                                        } else {
                                                            match crate::core::session_migration::convert_legacy_to_v4(&content) {
                                                                Ok(v4_content) => {
                                                                    let _ = std::fs::create_dir_all(&runtime.session_root);
                                                                    let converted = std::path::Path::new(&runtime.session_root)
                                                                        .join(format!("imported-{header_id}.jsonl"));
                                                                    match std::fs::write(&converted, v4_content) {
                                                                        Ok(()) => Some(converted.to_string_lossy().into_owned()),
                                                                        Err(e) => {
                                                                            status_banner = format!("import failed: {e}");
                                                                            None
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    status_banner = format!("import failed: {e}");
                                                                    None
                                                                }
                                                            }
                                                        };
                                                        let Some(resolved_path) = resolved_path else {
                                                            return Ok(());
                                                        };
                                                        let metadata = match crate::run::metadata_from_session_path(
                                                            std::path::Path::new(&resolved_path),
                                                        ) {
                                                            Ok(metadata) => metadata,
                                                            Err(error) => {
                                                                status_banner = format!("import failed: {error}");
                                                                return Ok(());
                                                            }
                                                        };
                                                        if !session_switch_allowed(
                                                            &runtime,
                                                            "resume",
                                                            Some(&metadata.path),
                                                        ) {
                                                            status_banner = "import cancelled by extension".to_string();
                                                        } else {
                                                        match runtime.repo.open(&metadata).await {
                                                            Ok(session) => {
                                                                let previous_session_file =
                                                                    runtime.session.get_metadata().await.path;
                                                                let target_session_file = session.get_metadata().await.path;
                                                                shutdown_extensions_before_session_replace(
                                                                    &runtime,
                                                                    "resume",
                                                                    Some(&target_session_file),
                                                                );
                                                                invalidate_interactive_harness(&mut runtime);
                                                                runtime.session = session;
                                                                runtime.session_id =
                                                                    runtime.session.get_metadata().await.id;
                                                                runtime.session_name = None;
                                                                let (messages, cache_entries) = rehydrate_transcript(
                                                                    &runtime,
                                                                    &transcript_md,
                                                                    hide_thinking,
                                                                )
                                                                .await;
                                                                runtime.messages = messages;
                                                                runtime.cache_entries = cache_entries;
                                                                runtime.persisted_until = runtime.messages.len();
                                                                status_log.clear();
                                                                clear_easter_egg_components(
                                                                    &mut easter_egg_components,
                                                                    &mut easter_egg_animation_until,
                                                                );
                                                                let notes = replace_extensions(
                                                                    &mut runtime,
                                                                    &settings,
                                                                    &thinking_level,
                                                                    "resume",
                                                                    Some(&previous_session_file),
                                                Some(&target_session_file),
                                            );
                                            refresh_startup_presentation(
                                                startup_presentation.as_ref(),
                                                &runtime,
                                                args,
                                                &settings,
                                            );
                                            status_banner = format!(
                                                                    "imported {} ({} prior messages)",
                                                                    path,
                                                                    runtime.messages.len()
                                                                );
                                                                if !notes.is_empty() {
                                                                    status_banner.push_str(&format!(
                                                                        " (extensions: {})",
                                                                        notes.join("; ")
                                                                    ));
                                                                }
                                                            }
                                                            Err(e) => {
                                                                status_banner = format!("import failed: {e}");
                                                            }
                                                        }
                                                        }
                                                    }
                                                }
                                            } else {
                                                status_banner = format!("cannot read {path}");
                                            }
                                        }
                                    }
                                    }
                                }
                                SlashKind::Reload => {
                                    invalidate_interactive_harness(&mut runtime);
                                    let theme_before = settings
                                        .get_theme_setting()
                                        .unwrap_or(crate::theme::DEFAULT_THEME)
                                        .to_string();
                                    settings.reload().await;
                                    refresh_interactive_retry_settings(&mut runtime, &settings);
                                    let mut notes: Vec<String> = Vec::new();
                                    for se in settings.drain_errors() {
                                        let where_ = se.path.clone().unwrap_or_else(|| format!("{:?}", se.scope));
                                        notes.push(format!("{where_}: {}", se.error));
                                    }
                                    let theme_after = settings
                                        .get_theme_setting()
                                        .unwrap_or(crate::theme::DEFAULT_THEME)
                                        .to_string();
                                    if theme_after != theme_before {
                                        notes.push(format!("theme changed to {theme_after}"));
                                    }
                                    notes.extend(reload_extensions(
                                        &mut runtime,
                                        &settings,
                                        &thinking_level,
                                    ));
                                    notes.extend(reload_interactive_models(&mut runtime));
                                    editor.lock().unwrap_or_else(|error| error.into_inner()).set_autocomplete_provider(Box::new(
                                        it::build_autocomplete_provider_with_skills(
                                            cwd.clone(),
                                            &runtime.skills,
                                            settings.get_enable_skill_commands(),
                                        ),
                                    ));
                                    if let Ok(mut mode) = mermaid_mode.lock() {
                                        *mode = settings.get_mermaid_rendering_mode().to_string();
                                    }
                                    register_interactive_themes(
                                        &runtime.extension_args,
                                        &settings,
                                        &runtime.extension_resources,
                                        &runtime.cwd,
                                    );
                                    load_interactive_theme_setting(
                                        settings
                                            .get_theme_setting()
                                            .unwrap_or(crate::theme::DEFAULT_THEME),
                                    );
                                    refresh_startup_presentation(
                                        startup_presentation.as_ref(),
                                        &runtime,
                                        args,
                                        &settings,
                                    );
                                    if notes.is_empty() {
                                        status_banner = "reloaded settings and extensions".to_string();
                                    } else {
                                        status_banner = format!("reloaded settings ({})", notes.join("; "));
                                    }
                                }
                                SlashKind::Fork => {
                                    if !runtime.session_persistence {
                                        status_banner =
                                            "fork requires a persistent session; remove --no-session"
                                                .to_string();
                                    } else {
                                        let items = fork_selector_items(&runtime.session).await;
                                        if items.is_empty() {
                                            status_banner = "No messages to fork from".to_string();
                                        } else {
                                            let last = items.len().saturating_sub(1);
                                            let mut selector = ListSelector::new_slash_layout(items, 10);
                                            selector.set_selected_index(last);
                                            modal = Some(Modal::Fork(Arc::new(Mutex::new(selector))));
                                        }
                                    }
                                }
                                SlashKind::Clone => {
                                    if !runtime.session_persistence {
                                        status_banner =
                                            "clone requires a persistent session; remove --no-session"
                                                .to_string();
                                    } else {
                                    match runtime.session.get_leaf_id().await.ok().flatten() {
                                        Some(entry_id) => {
                                            let result = execute_interactive_fork(
                                                &mut runtime,
                                                "clone",
                                                entry_id,
                                                ForkPosition::At,
                                                InteractiveForkContext {
                                                    settings: &settings,
                                                    thinking_level: &thinking_level,
                                                    transcript_md: &transcript_md,
                                                    hide_thinking,
                                                },
                                                None,
                                            )
                                            .await;
                                            refresh_startup_presentation(
                                                startup_presentation.as_ref(),
                                                &runtime,
                                                args,
                                                &settings,
                                            );
                                            if let Some(text) = result.editor_text {
                                                editor.lock().unwrap_or_else(|error| error.into_inner()).set_text(&text);
                                            }
                                            status_log.clear();
                                            status_banner = result.status;
                                        }
                                        None => {
                                            status_banner = "Nothing to clone yet".to_string();
                                        }
                                    }
                                    }
                                }
                                SlashKind::Trust => {
                                    let choice = arg
                                        .as_deref()
                                        .map(|s| s.trim().to_lowercase())
                                        .filter(|s| !s.is_empty());
                                    match choice {
                                        None => {
                                            // `/trust` is the project-scoped
                                            // selector in upstream Pi. Read
                                            // the saved nearest-ancestor
                                            // decision without panicking so a
                                            // malformed/readonly trust store
                                            // returns to the editor with an
                                            // actionable status message.
                                            let trust_store =
                                                crate::core::project_trust::ProjectTrustStore::new(
                                                    &runtime.extension_agent_dir,
                                                );
                                            match trust_store.try_get_entry(&runtime.cwd) {
                                                Ok(saved_decision) => {
                                                    modal = Some(Modal::Trust(Arc::new(
                                                        Mutex::new(
                                                            it::selectors::TrustSelector::new(
                                                                runtime.cwd.clone(),
                                                                saved_decision,
                                                                settings.is_project_trusted(),
                                                            ),
                                                        ),
                                                    )));
                                                }
                                                Err(error) => {
                                                    status_banner =
                                                        format!("Could not open project trust: {error}");
                                                }
                                            }
                                        }
                                        Some(choice)
                                            if matches!(choice.as_str(), "allow" | "deny" | "ask") =>
                                        {
                                            // Keep the explicit policy form as
                                            // a compatibility shortcut; the
                                            // no-argument command above is the
                                            // project decision selector.
                                            let trust = match choice.as_str() {
                                                "allow" => "always",
                                                "deny" => "never",
                                                _ => "ask",
                                            };
                                            settings.set_default_project_trust(trust);
                                            status_banner = format!("default project trust: {choice}");
                                        }
                                        Some(_) => {
                                            status_banner = "usage: /trust [allow|deny|ask]".to_string();
                                        }
                                    }
                                }
                                SlashKind::Copy => {
                                    // Copy the last assistant message through the same real
                                    // backend stack as Ctrl+V. A missing backend is reported;
                                    // the old preview-only behavior falsely claimed success.
                                    let mut text = String::new();
                                    for message in runtime.messages.iter().rev() {
                                        if let pi_agent::types::AgentMessage::Core(
                                            pi_ai::types::Message::Assistant(a),
                                        ) = message
                                        {
                                            for block in a.content() {
                                                if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                                                    if !t.is_empty() {
                                                        text = t.clone();
                                                        break;
                                                    }
                                                }
                                            }
                                            break;
                                        }
                                    }
                                    if text.is_empty() {
                                        status_banner = "no assistant message to copy".to_string();
                                    } else {
                                        input.stop_worker().await;
                                        let copied = it::clipboard::copy_to_clipboard(&text).await;
                                        input.restart().await;
                                        status_banner = match copied {
                                            Ok(()) => "copied last assistant message to clipboard".to_string(),
                                            Err(error) => format!("copy failed: {}", error.0),
                                        };
                                    }
                                }
                                SlashKind::Tree => {
                                    let terminal_height = renderer
                                        .terminal_handle()
                                        .lock().unwrap_or_else(|error| error.into_inner())
                                        .height();
                                    match tree_selector_for_session(
                                        &runtime.session,
                                        terminal_height,
                                        it::tree_selector::TreeFilterMode::from_setting(
                                            settings.get_tree_filter_mode(),
                                        ),
                                    )
                                    .await
                                    {
                                        Err(error) => status_banner = error.to_string(),
                                        Ok(None) => status_banner = "No entries in session".to_string(),
                                        Ok(Some(selector)) => {
                                            modal = Some(Modal::Tree(Arc::new(Mutex::new(selector))));
                                        }
                                    }
                                }
                                SlashKind::Share => {
                                    // Persist unpersisted messages so the exported HTML
                                    // matches the current transcript.
                                    if runtime.messages.len() > runtime.persisted_until {
                                        let to_append: Vec<pi_agent::types::AgentMessage> =
                                            runtime.messages[runtime.persisted_until..].to_vec();
                                        persist_messages(&mut runtime.session, &to_append).await;
                                        runtime.persisted_until = runtime.messages.len();
                                    }
                                    let dry_run = std::env::var("PI_SHARE_DRY_RUN").as_deref() == Ok("1");
                                    match run_share(&runtime, dry_run).await {
                                        Ok(message) => status_banner = message,
                                        Err(e) => status_banner = e,
                                    }
                                }
                        }
                    }
                }
            }
            if !had_submission && immediately_repainted {
                skip_owner_render_after_immediate_repaint = true;
            }
        }
    })
    .await;

    input.shutdown().await;

    // Persist messages that were added after the last session-switch
    // operation (resume/fork/clone advance the watermark; the rest already
    // live in the session).
    if runtime.messages.len() > runtime.persisted_until {
        let to_append: Vec<pi_agent::types::AgentMessage> =
            runtime.messages[runtime.persisted_until..].to_vec();
        persist_messages(&mut runtime.session, &to_append).await;
    }

    let fullscreen_exit_output = settings.get_fullscreen_exit_output().to_string();
    let final_scene = if use_alt_screen && fullscreen_exit_output == "transcript" {
        // Upstream hides overlays before switching to the regular renderer.
        // The Rust scene is rebuilt without the modal component for the same
        // clean transcript projection.
        let scene = it::build_interactive_scene_with_loader_and_scroll_view(
            &transcript_scroll_view,
            &editor,
            &footer_text,
            Some(&status_text),
            None,
            &easter_egg_components,
            &pending_loader,
            &pending_text,
        );
        Some(scene)
    } else {
        None
    };

    renderer.stop(&fullscreen_exit_output, final_scene.as_ref());

    if !startup_resume_cancelled && use_alt_screen && fullscreen_exit_output == "resume-hint" {
        if let Some(command) = format_resume_command(&runtime).await {
            println!(
                "{}",
                it::tui_theme::dim(format!("To resume this session: {command}"))
            );
        }
    }
    if startup_resume_cancelled {
        println!("No session selected");
    } else if startup_cross_project_cancelled {
        println!("Aborted.");
    }
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("interactive mode timed out".to_string()),
    }
}

/// The terminal reader may coalesce adjacent printable input.  Named key
/// strings are also printable ASCII, so exclude the complete key-string
/// vocabulary before treating a multi-character event as text.
fn interactive_editor_input<'a>(raw: &'a str, key: &TuiKey) -> &'a str {
    // tmux and several xterm-compatible terminals encode Alt+Enter as ESC LF,
    // while the legacy Return encoding is ESC CR. The shared parser exposes
    // the former as an Alt-modified newline; normalize both to ordinary Enter
    // for the idle editor (streaming mode handles the queue action above).
    if is_alt_enter_key(raw, key) {
        return "enter";
    }

    // In legacy mode, LF is the ordinary Return encoding. `Editor` must see
    // the canonical key name so its legacy Shift+Enter compatibility branch
    // does not consume the Return as a newline. When Kitty is active the
    // parser marks LF as Shift+Enter and the raw byte must remain unchanged.
    if raw == "\n" && key.base == "enter" && !key.ctrl && !key.alt && !key.shift {
        "enter"
    } else {
        raw
    }
}

fn is_alt_enter_key(raw: &str, key: &TuiKey) -> bool {
    key.alt
        && !key.ctrl
        && !key.shift
        && ((key.base == "enter" && matches!(raw, "\x1b\r" | "\x1b[13;3u"))
            || (key.base == "\n" && raw == "\x1b\n"))
}

/// Return true for the normal Alt+Enter sequence. When Kitty keyboard
/// protocol is active, the ambiguous legacy `ESC CR` mapping is Shift+Enter
/// and must remain a multiline editor action, matching Pi's `matchesKey`
/// behavior; real Alt+Enter arrives as CSI-u (or modifyOtherKeys fallback).
fn is_streaming_follow_up_key(raw: &str, key: &TuiKey) -> bool {
    is_alt_enter_key(raw, key)
}

fn is_printable_input_batch(raw: &str, key: &TuiKey) -> bool {
    raw.chars().count() > 1
        && !raw.contains('\x1b')
        && raw.chars().all(|character| !character.is_control())
        && !key.ctrl
        && !key.alt
        && !key.shift
        && !matches!(
            raw,
            "enter"
                | "esc"
                | "escape"
                | "backspace"
                | "delete"
                | "tab"
                | "shift+tab"
                | "up"
                | "down"
                | "left"
                | "right"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
                | "f1"
                | "f2"
                | "f3"
                | "f4"
        )
}

fn is_immediate_editor_input(raw: &str, key: &TuiKey) -> bool {
    if key.ctrl || key.alt || key.shift || key.super_key || raw.contains('\x1b') {
        return false;
    }
    is_printable_input_batch(raw, key)
        || (raw.chars().count() == 1 && raw.chars().all(|character| !character.is_control()))
}

fn take_immediate_editor_key(
    event: Result<pi_tui::terminal::TerminalEvent, String>,
    pending_events: &mut VecDeque<Result<pi_tui::terminal::TerminalEvent, String>>,
) -> Option<String> {
    match event {
        Ok(pi_tui::terminal::TerminalEvent::Key(raw)) => {
            let key = parse_key(&raw);
            if is_immediate_editor_input(&raw, &key) {
                Some(raw)
            } else {
                pending_events.push_front(Ok(pi_tui::terminal::TerminalEvent::Key(raw)));
                None
            }
        }
        other => {
            pending_events.push_front(other);
            None
        }
    }
}

/// Requeue a coalesced printable terminal event as one scalar key per event
/// before handing it to a modal. Modal search fields consume a single
/// printable key at a time, while the terminal reader intentionally combines
/// adjacent text for the editor fast path. Reverse insertion keeps the
/// original order and puts the expanded keys ahead of any already-queued
/// terminal event. Control/escape sequences never reach this helper because
/// [`is_printable_input_batch`] rejects them.
fn enqueue_modal_printable_batch(
    pending_events: &mut VecDeque<Result<pi_tui::terminal::TerminalEvent, String>>,
    text: &str,
) {
    let keys: Vec<Result<pi_tui::terminal::TerminalEvent, String>> = text
        .chars()
        .map(|character| Ok(pi_tui::terminal::TerminalEvent::Key(character.to_string())))
        .collect::<Vec<_>>();
    for key in keys.into_iter().rev() {
        pending_events.push_front(key);
    }
}

/// Restore the editor state that Pi puts back after aborting a streaming run.
/// Queued steering messages precede queued follow-ups, followed by any draft
/// that had not been submitted when cancellation arrived.
fn restore_interactive_queued_input(editor: &mut Editor, queued: &[InteractivePendingMessage]) {
    let mut restored = queued
        .iter()
        .filter(|message| message.kind == InteractiveQueueKind::Steering)
        .chain(
            queued
                .iter()
                .filter(|message| message.kind == InteractiveQueueKind::FollowUp),
        )
        .map(|message| message.text.as_str())
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
    let current = editor.get_text();
    if !current.trim().is_empty() {
        restored.push(current.as_str());
    }
    editor.set_text(&restored.join("\n\n"));
}

/// Keep fast PTY/paste bursts responsive.  Feeding a large plain-text event
/// through the per-grapheme editor path repeatedly clones the growing line and
/// can delay the following Return/Alt+Enter event long enough to make a live
/// turn appear unresponsive.  The editor's bulk insertion path preserves the
/// text and undo boundary; smaller events retain the normal per-grapheme path
/// so slash/autocomplete typing semantics stay unchanged.
fn insert_interactive_text_batch(editor: &mut Editor, text: &str) {
    if text.len() > 1024 {
        editor.insert_text_at_cursor(text);
    } else {
        editor.handle_input_burst(text);
    }
}

fn apply_interactive_editor_input(editor: &Arc<Mutex<Editor>>, raw: &str, key: &TuiKey) {
    let editor_input = interactive_editor_input(raw, key);
    let mut editor = editor.lock().unwrap_or_else(|error| error.into_inner());
    if is_printable_input_batch(raw, key) {
        insert_interactive_text_batch(&mut editor, raw);
    } else {
        editor.handle_input(editor_input);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::extensions::{ExtensionHostAction, ExtensionHostActions};
    use pi_agent::fs::StdFileSystem;
    use pi_agent::session::jsonl::repo::CreateOptions;
    use pi_agent::session::state::ForkOptions;
    use pi_agent::session::JsonlSessionRepo;

    #[test]
    fn interactive_explicit_skills_survive_no_skills() {
        let root =
            std::env::temp_dir().join(format!("pi-interactive-no-skills-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        let automatic = agent_dir.join("skills/automatic/SKILL.md");
        let explicit = root.join("explicit/SKILL.md");
        std::fs::create_dir_all(automatic.parent().unwrap()).unwrap();
        std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            &automatic,
            "---\nname: automatic\ndescription: automatic skill\n---\nautomatic body\n",
        )
        .unwrap();
        std::fs::write(
            &explicit,
            "---\nname: explicit\ndescription: explicit skill\n---\nexplicit body\n",
        )
        .unwrap();
        let args = Args {
            no_skills: true,
            skills: vec![explicit.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let settings = SettingsManager::in_memory(Default::default());

        let skills = load_interactive_skills(
            &args,
            &cwd.to_string_lossy(),
            &agent_dir.to_string_lossy(),
            &settings,
            &ResourceDiscovery::default(),
        );
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["explicit"]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ctrl_d_exits_only_for_an_empty_editor() {
        let ctrl_d = parse_key("\x04");
        assert!(should_exit_on_key(&ctrl_d, ""));
        assert!(!should_exit_on_key(&ctrl_d, "draft"));
        assert!(!should_exit_on_key(&parse_key("ctrl+c"), ""));
    }

    #[test]
    fn missing_session_id_warning_matches_upstream_diagnostic() {
        assert_eq!(
            missing_session_id_warning("session-1"),
            "Warning: No project session found with id 'session-1'; creating a new session with that id."
        );
    }

    #[test]
    fn working_loader_uses_upstream_label_for_each_turn_kind() {
        assert_eq!(
            interactive_working_message(InteractiveQueueKind::Steering),
            "Working..."
        );
        assert_eq!(
            interactive_working_message(InteractiveQueueKind::FollowUp),
            "Working..."
        );
    }

    #[test]
    fn model_scope_startup_notice_is_gated_and_keeps_thinking_overrides() {
        let scoped_models = vec![
            crate::core::model_resolver::ScopedModel {
                model: Model::new("faux-1", "Faux 1", "faux", "faux"),
                thinking_level: Some("high".to_string()),
            },
            crate::core::model_resolver::ScopedModel {
                model: Model::new("faux-2", "Faux 2", "faux", "faux"),
                thinking_level: None,
            },
        ];

        assert_eq!(
            model_scope_startup_message(&scoped_models, true, true).as_deref(),
            Some("Model scope: faux-1:high, faux-2 (Ctrl+P to cycle)")
        );
        assert!(model_scope_startup_message(&scoped_models, false, true).is_none());
        assert!(model_scope_startup_message(&[], true, false).is_none());
    }

    #[test]
    fn retained_markdown_forwards_streaming_context_to_mermaid() {
        let mode = Arc::new(Mutex::new("streaming".to_string()));
        let is_streaming = Arc::new(AtomicBool::new(false));
        let source = "```mermaid\nflowchart LR\nA[Foo]:::highlight --> B[Bar]\n```";
        let mut view = InteractiveTranscriptView::new(mode, Arc::clone(&is_streaming));
        view.set_blocks(vec![InteractiveTranscriptBlock::Markdown(
            source.to_string(),
        )]);

        let final_text = pi_tui::strip_ansi_codes(&view.render(100).join("\n"));
        assert!(final_text.contains("```mermaid"));
        assert!(final_text.contains("Mermaid diagram not rendered"));

        is_streaming.store(true, Ordering::Release);
        view.invalidate();
        let streaming_text = pi_tui::strip_ansi_codes(&view.render(100).join("\n"));
        assert!(!streaming_text.contains("```mermaid"));
        assert!(streaming_text.contains("Foo"));
    }

    #[test]
    fn double_escape_routes_only_inside_the_500ms_window() {
        let start = std::time::Instant::now();
        let mut last_escape = None;
        assert_eq!(resolve_double_escape("tree", &mut last_escape, start), None);
        assert_eq!(
            resolve_double_escape(
                "tree",
                &mut last_escape,
                start + std::time::Duration::from_millis(499)
            ),
            Some(DoubleEscapeAction::Tree)
        );
        assert!(last_escape.is_none());

        assert_eq!(resolve_double_escape("fork", &mut last_escape, start), None);
        assert_eq!(
            resolve_double_escape(
                "fork",
                &mut last_escape,
                start + std::time::Duration::from_millis(500)
            ),
            None
        );
        assert_eq!(
            resolve_double_escape(
                "fork",
                &mut last_escape,
                start + std::time::Duration::from_millis(700)
            ),
            Some(DoubleEscapeAction::Fork)
        );

        assert_eq!(resolve_double_escape("none", &mut last_escape, start), None);
        assert!(last_escape.is_none());

        assert_eq!(resolve_double_escape("tree", &mut last_escape, start), None);
        last_escape = None;
        // A key other than Escape is handled by the loop by clearing the
        // arm; a later Escape must therefore start a fresh pair.
        assert_eq!(resolve_double_escape("tree", &mut last_escape, start), None);
        assert_eq!(
            resolve_double_escape(
                "tree",
                &mut last_escape,
                start + std::time::Duration::from_millis(501)
            ),
            None
        );
    }

    #[test]
    fn settings_queue_modes_update_the_retained_agent_in_place() {
        let stream_fn: crate::run::StreamFn = std::sync::Arc::new(|_, _| {
            panic!("settings queue mode test must not start a provider stream")
        });
        let agent = pi_agent::rich_agent::Agent::new(stream_fn);
        assert_eq!(
            agent.steering_mode(),
            pi_agent::rich_agent::QueueMode::OneAtATime
        );
        assert_eq!(
            agent.follow_up_mode(),
            pi_agent::rich_agent::QueueMode::OneAtATime
        );

        apply_interactive_queue_mode(&agent, "steering-mode", "all");
        apply_interactive_queue_mode(&agent, "follow-up-mode", "all");

        assert_eq!(agent.steering_mode(), pi_agent::rich_agent::QueueMode::All);
        assert_eq!(agent.follow_up_mode(), pi_agent::rich_agent::QueueMode::All);

        apply_interactive_queue_mode(&agent, "steering-mode", "one-at-a-time");
        apply_interactive_queue_mode(&agent, "follow-up-mode", "one-at-a-time");
        assert_eq!(
            agent.steering_mode(),
            pi_agent::rich_agent::QueueMode::OneAtATime
        );
        assert_eq!(
            agent.follow_up_mode(),
            pi_agent::rich_agent::QueueMode::OneAtATime
        );
    }

    #[test]
    fn bash_completion_requeues_the_key_that_arrived_during_completion() {
        let mut pending = VecDeque::from([Ok(pi_tui::terminal::TerminalEvent::Key(
            "later".to_string(),
        ))]);

        defer_input_until_bash_completion(&mut pending, "!!printf next".to_string());

        assert_eq!(
            pending.pop_front(),
            Some(Ok(pi_tui::terminal::TerminalEvent::Key(
                "!!printf next".to_string()
            )))
        );
        assert_eq!(
            pending.pop_front(),
            Some(Ok(pi_tui::terminal::TerminalEvent::Key(
                "later".to_string()
            )))
        );
    }

    #[test]
    fn modal_printable_batch_requeues_each_query_character_in_order() {
        let raw = "faux-1";
        let key = parse_key(raw);
        assert!(is_printable_input_batch(raw, &key));
        assert!(!is_printable_input_batch("faux\n", &parse_key("faux\n")));
        assert!(!is_printable_input_batch("ctrl+c", &parse_key("ctrl+c")));

        let mut pending = VecDeque::from([Ok(pi_tui::terminal::TerminalEvent::Key(
            "later".to_string(),
        ))]);
        enqueue_modal_printable_batch(&mut pending, raw);

        let keys = pending
            .into_iter()
            .map(|event| match event.unwrap() {
                pi_tui::terminal::TerminalEvent::Key(key) => key,
                pi_tui::terminal::TerminalEvent::Resize(_, _) => {
                    panic!("modal printable batch emitted a resize")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "f".to_string(),
                "a".to_string(),
                "u".to_string(),
                "x".to_string(),
                "-".to_string(),
                "1".to_string(),
                "later".to_string(),
            ]
        );
    }

    #[test]
    fn kitty_alt_enter_uses_csi_u_while_legacy_escape_return_is_shift_enter() {
        pi_tui::keys::set_kitty_protocol_active(false);
        let key = parse_key("\x1b\r");
        assert!(is_streaming_follow_up_key("\x1b\r", &key));

        pi_tui::keys::set_kitty_protocol_active(true);
        let legacy_key = parse_key("\x1b\r");
        assert!(!is_streaming_follow_up_key("\x1b\r", &legacy_key));
        let kitty_key = parse_key("\x1b[13;3u");
        assert!(is_streaming_follow_up_key("\x1b[13;3u", &kitty_key));
        pi_tui::keys::set_kitty_protocol_active(false);
    }

    #[test]
    fn interrupted_stream_restores_queued_messages_before_current_draft() {
        let mut editor = it::create_editor(".".to_string());
        editor.set_text("current draft");
        let queued = vec![
            InteractivePendingMessage {
                text: "follow-up".to_string(),
                kind: InteractiveQueueKind::FollowUp,
            },
            InteractivePendingMessage {
                text: "steering".to_string(),
                kind: InteractiveQueueKind::Steering,
            },
        ];

        restore_interactive_queued_input(&mut editor, &queued);

        assert_eq!(editor.get_text(), "steering\n\nfollow-up\n\ncurrent draft");
    }

    #[test]
    fn cached_scene_repaint_reflects_editor_input_before_owner_preparation() {
        let terminal = Arc::new(Mutex::new(TerminalBackend::new_with_size(32, 6)));
        let mut renderer = InteractiveRenderer::new(terminal.clone(), false);
        let editor = Arc::new(Mutex::new(it::create_editor(".".to_string())));
        renderer.focus(editor.clone() as SharedComponent);
        let scene = Arc::new(Mutex::new(Scene::new(
            vec![editor.clone() as SharedComponent],
            None,
        )));

        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        renderer.render_scene(&scene);
        let _ = terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take_output_capture();

        editor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle_input("a");
        terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_output_capture();
        assert!(renderer.render_cached_scene(Some(&scene)));
        let output = String::from_utf8(
            terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take_output_capture(),
        )
        .expect("captured terminal output is UTF-8");
        assert!(output.contains('a'));
    }

    #[test]
    fn immediate_editor_queue_coalesces_text_but_preserves_control_order() {
        let (_sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut input = InteractiveInputReader {
            terminal: Arc::new(Mutex::new(TerminalBackend::new_with_size(32, 6))),
            receiver,
            pending_events: VecDeque::from([
                Ok(pi_tui::terminal::TerminalEvent::Key("ab日本語".to_string())),
                Ok(pi_tui::terminal::TerminalEvent::Key("cd".to_string())),
                Ok(pi_tui::terminal::TerminalEvent::Key("\r".to_string())),
            ]),
            stop: Arc::new(AtomicBool::new(false)),
            task: None,
        };
        let keys = input.take_immediate_editor_keys();
        assert_eq!(keys, vec!["ab日本語", "cd"]);
        assert_eq!(input.pending_events.len(), 1);
        assert!(matches!(
            input.pending_events.front(),
            Some(Ok(pi_tui::terminal::TerminalEvent::Key(raw))) if raw == "\r"
        ));

        let mut pending = VecDeque::new();
        let first = take_immediate_editor_key(
            Ok(pi_tui::terminal::TerminalEvent::Key("ab日本語".to_string())),
            &mut pending,
        );
        assert_eq!(first.as_deref(), Some("ab日本語"));
        assert!(pending.is_empty());

        let control = take_immediate_editor_key(
            Ok(pi_tui::terminal::TerminalEvent::Key("\r".to_string())),
            &mut pending,
        );
        assert_eq!(control, None);
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending.front(),
            Some(Ok(pi_tui::terminal::TerminalEvent::Key(raw))) if raw == "\r"
        ));
    }

    #[test]
    fn immediate_editor_input_excludes_modifiers_and_terminal_sequences() {
        assert!(is_immediate_editor_input("a", &parse_key("a")));
        assert!(is_immediate_editor_input("日本語", &parse_key("日本語")));
        assert!(is_immediate_editor_input(
            "rapid text",
            &parse_key("rapid text")
        ));
        for raw in ["\r", "\n", "enter", "\x1b", "\x1b[A", "ctrl+c", "alt+x"] {
            assert!(
                !is_immediate_editor_input(raw, &parse_key(raw)),
                "non-printable input was admitted to the immediate editor path: {raw:?}"
            );
        }
    }

    #[test]
    fn provider_error_is_rendered_once_in_transcript_not_status_banner() {
        let mut assistant = pi_ai::types::AssistantMessage::new();
        assistant.set_stop_reason(pi_ai::types::StopReason::Error);
        assistant.set_error_message("OpenAI 401: invalid authentication");
        let message =
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(assistant));
        let rendered = it::messages::render_message(&message, false)
            .expect("provider error should remain visible")
            .1;
        assert_eq!(rendered.matches("OpenAI 401").count(), 1);
        assert_eq!(interactive_turn_error_banner(&Ok(vec![message])), None);
        assert_eq!(
            interactive_turn_error_banner(&Err("transport failed".to_string())),
            Some("transport failed".to_string())
        );
    }

    #[test]
    fn editor_border_tracks_thinking_and_bash_modes() {
        it::tui_theme::load_theme(crate::theme::DEFAULT_THEME);
        let editor = Arc::new(Mutex::new(it::create_editor(".".to_string())));
        let mut last_state = None;

        sync_editor_border(&editor, "medium", false, &mut last_state);
        assert_eq!(
            (editor
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .border_color)("─"),
            it::tui_theme::thinking_border("medium")("─")
        );

        editor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_text("! printf hello");
        sync_editor_border(&editor, "medium", true, &mut last_state);
        assert_eq!(
            (editor
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .border_color)("─"),
            it::tui_theme::bash_mode_border()("─")
        );

        editor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_text("");
        sync_editor_border(&editor, "high", false, &mut last_state);
        assert_eq!(
            (editor
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .border_color)("─"),
            it::tui_theme::thinking_border("high")("─")
        );
    }

    #[test]
    fn debug_timestamp_matches_upstream_iso_shape() {
        let timestamp = iso_timestamp_now();
        assert_eq!(timestamp.len(), 24);
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[7..8], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[13..14], ":");
        assert_eq!(&timestamp[16..17], ":");
        assert_eq!(&timestamp[19..20], ".");
        assert_eq!(&timestamp[23..24], "Z");
    }

    #[tokio::test]
    async fn resume_candidates_exclude_the_current_session() {
        let root =
            std::env::temp_dir().join(format!("pi-resume-empty-selector-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let runtime = test_runtime(&root).await;
        let sessions = runtime.repo.list(Some(&runtime.cwd)).await.unwrap();
        assert!(resumable_sessions(sessions, &runtime.session_id, None).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_fork_hook_returns_cancellation_without_mutating_session() {
        let root =
            std::env::temp_dir().join(format!("pi-interactive-fork-hook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let original_session_id = runtime.session_id.clone();
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let events_for_handler = Arc::clone(&events);
        let handler = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, event: &Value| {
                events_for_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(Some(json!({"cancel": true})))
            },
        ) as crate::core::extensions::HandlerFn;
        let mut extension = crate::core::extensions::Extension {
            path: "interactive-fork-hook.js".to_string(),
            ..Default::default()
        };
        extension
            .handlers
            .insert("session_before_fork".to_string(), vec![handler]);
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        runtime.extensions = LoadedExtensions {
            runner: Arc::new(crate::core::extensions::ExtensionRunner::new(
                vec![extension],
                Arc::clone(&extension_runtime),
                runtime.cwd.clone(),
            )),
            host: Arc::new(crate::core::extensions::ExtensionHostState::new(
                None, "off",
            )),
            runtime: extension_runtime,
            errors: Vec::new(),
            resources: ResourceDiscovery::default(),
        };
        assert!(!session_fork_allowed(&runtime, "leaf-entry", "at"));
        assert_eq!(runtime.session_id, original_session_id);
        assert_eq!(
            *events.lock().unwrap_or_else(|error| error.into_inner()),
            vec![json!({
                "type": "session_before_fork",
                "entryId": "leaf-entry",
                "position": "at"
            })]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_extension_commands_run_before_unknown_slash_falls_back_to_prompt() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-extension-command-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let seen_args = Arc::new(Mutex::new(String::new()));
        let seen_args_for_handler = Arc::clone(&seen_args);
        let handler = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, event: &Value| {
                *seen_args_for_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    event["args"].as_str().unwrap_or_default().to_string();
                Ok(Some(json!({"ok": true})))
            },
        ) as crate::core::extensions::HandlerFn;
        let mut extension = crate::core::extensions::Extension {
            path: "interactive-command.js".to_string(),
            ..Default::default()
        };
        extension.commands.insert(
            "hello".to_string(),
            crate::core::extensions::types::RegisteredCommand {
                name: "hello".to_string(),
                source_info: Default::default(),
                description: Some("test command".to_string()),
                handler,
            },
        );
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        runtime.extensions = LoadedExtensions {
            runner: Arc::new(crate::core::extensions::ExtensionRunner::new(
                vec![extension],
                Arc::clone(&extension_runtime),
                runtime.cwd.clone(),
            )),
            host: Arc::new(crate::core::extensions::ExtensionHostState::new(
                None, "off",
            )),
            runtime: extension_runtime,
            errors: Vec::new(),
            resources: ResourceDiscovery::default(),
        };

        assert_eq!(
            execute_interactive_extension_command(&runtime, "/hello first second"),
            Some("/hello: {\"ok\":true}".to_string())
        );
        assert_eq!(
            *seen_args.lock().unwrap_or_else(|error| error.into_inner()),
            "first second"
        );
        assert!(execute_interactive_extension_command(&runtime, "/model faux/faux-1").is_none());

        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn interactive_lifecycle_action_creates_a_real_session() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-extension-new-session-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let previous_id = runtime.session_id.clone();
        let lifecycle_events = Arc::new(Mutex::new(Vec::<String>::new()));
        let before_events = Arc::clone(&lifecycle_events);
        let shutdown_events = Arc::clone(&lifecycle_events);
        let mut extension = crate::core::extensions::Extension {
            path: "interactive-session-lifecycle.js".to_string(),
            ..Default::default()
        };
        let before_handler: crate::core::extensions::HandlerFn = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, _: &Value| {
                before_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push("before_switch".to_string());
                Ok(None)
            },
        );
        extension
            .handlers
            .insert("session_before_switch".to_string(), vec![before_handler]);
        let shutdown_handler: crate::core::extensions::HandlerFn = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, _: &Value| {
                shutdown_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push("shutdown".to_string());
                Ok(None)
            },
        );
        extension
            .handlers
            .insert("session_shutdown".to_string(), vec![shutdown_handler]);
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        runtime.extensions = LoadedExtensions {
            runner: Arc::new(crate::core::extensions::ExtensionRunner::new(
                vec![extension],
                Arc::clone(&extension_runtime),
                runtime.cwd.clone(),
            )),
            host: Arc::new(crate::core::extensions::ExtensionHostState::new(
                None, "off",
            )),
            runtime: extension_runtime,
            errors: Vec::new(),
            resources: ResourceDiscovery::default(),
        };
        runtime
            .extensions
            .host
            .dispatch_with_outcome(ExtensionHostAction::NewSession, &json!({"options": {}}))
            .unwrap();
        let transcript_md = Arc::new(Mutex::new(Markdown::new(
            String::new(),
            1,
            0,
            it::tui_theme::markdown_theme(),
            None,
            None,
        )));
        let mut settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let notes = apply_pending_extension_lifecycle_actions(
            &mut runtime,
            &mut settings,
            "off",
            &transcript_md,
            false,
        )
        .await;

        assert_ne!(runtime.session_id, previous_id);
        assert_eq!(runtime.messages, Vec::new());
        assert!(notes
            .iter()
            .any(|note| note.contains("started new session")));
        assert_eq!(
            *lifecycle_events
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            vec!["before_switch", "shutdown"]
        );
        let session_path = runtime.session.get_metadata().await.path;
        assert!(std::path::Path::new(&session_path).is_file());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn interactive_replacements_teardown_before_session_assignment() {
        let source = include_str!("interactive.rs");
        let mut last_shutdown = 0usize;
        let mut assignments = 0usize;
        for (line_number, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("shutdown_extensions_before_session_replace(") {
                last_shutdown = line_number;
            }
            if trimmed.starts_with("runtime.session =") {
                assignments += 1;
                assert!(
                    line_number.saturating_sub(last_shutdown) <= 40,
                    "session assignment at line {} must follow old-runner teardown",
                    line_number + 1
                );
            }
        }
        assert_eq!(
            assignments, 7,
            "all interactive replacement paths are covered"
        );
    }

    #[tokio::test]
    async fn startup_presentation_refreshes_runtime_resources_and_keeps_expansion() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-startup-refresh-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let runtime = test_runtime(&root).await;
        let args = Args::default();
        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let startup = Arc::new(Mutex::new(
            it::startup::InteractiveStartupPresentation::new(
                crate::config::VERSION,
                &runtime.cwd,
                &runtime.extension_agent_dir,
                &args,
                &settings,
                &runtime.extension_resources,
                runtime.extensions.runner.extensions(),
                &runtime.extensions.errors,
                &runtime.prompt_templates,
                false,
            ),
        ));
        startup
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_expanded(true);

        let mut runtime = runtime;
        runtime
            .prompt_templates
            .push(crate::core::prompt_templates::PromptTemplate {
                name: "review".to_string(),
                description: "Review the current change".to_string(),
                argument_hint: None,
                content: "Review".to_string(),
                source_info: crate::core::extensions::SourceInfo::default(),
                file_path: root.join("review.md").to_string_lossy().into_owned(),
            });
        refresh_startup_presentation(Some(&startup), &runtime, &args, &settings);

        let startup_guard = startup.lock().unwrap_or_else(|error| error.into_inner());
        assert!(startup_guard.is_expanded());
        assert!(
            pi_tui::strip_ansi_codes(&startup_guard.render(120).join("\n")).contains("/review")
        );

        drop(startup_guard);
        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn extension_turn_changes_apply_to_tools_and_idle_state() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-extension-turn-changes-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let requested_model = serde_json::to_value(&runtime.model).unwrap();
        runtime
            .extensions
            .host
            .dispatch(
                ExtensionHostAction::SetModel,
                &json!({"model": requested_model}),
            )
            .unwrap();
        runtime
            .extensions
            .host
            .dispatch(
                ExtensionHostAction::SetActiveTools,
                &json!({"toolNames": ["read"]}),
            )
            .unwrap();

        apply_extension_turn_changes(&mut runtime);
        let tools = interactive_turn_tools(&runtime);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read"]
        );
        assert!(runtime.extensions.host.requested_model().is_none());
        assert!(runtime.extensions.host.requested_active_tools().is_none());

        let idle_guard = InteractiveIdleGuard::new(runtime.extensions.host.clone());
        assert!(runtime
            .extensions
            .host
            .wait_for_idle_timeout(std::time::Duration::from_millis(1))
            .is_err());
        drop(idle_guard);
        assert!(runtime
            .extensions
            .host
            .wait_for_idle_timeout(std::time::Duration::from_millis(1))
            .is_ok());

        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn interactive_extension_themes_retain_source_info() {
        let _lock = crate::theme::test_theme_registry_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-theme-source-info-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("extension-theme.json");
        let name = format!("interactive-extension-theme-{}", uuid::Uuid::new_v4());
        let mut theme: Value = serde_json::from_str(include_str!("../../data/themes/dark.json"))
            .expect("builtin fixture parses");
        theme["name"] = Value::String(name.clone());
        std::fs::write(&path, serde_json::to_vec(&theme).unwrap()).unwrap();

        let source_info = crate::core::extensions::SourceInfo {
            path: root.join("extension.ts").to_string_lossy().into_owned(),
            source: "extension:theme-fixture".to_string(),
            scope: "temporary".to_string(),
            origin: "top-level".to_string(),
            base_dir: Some(root.to_string_lossy().into_owned()),
        };
        let resources = ResourceDiscovery {
            theme_resources: vec![crate::core::extensions::DiscoveredResource {
                path: path.to_string_lossy().into_owned(),
                extension_path: source_info.path.clone(),
                source_info: source_info.clone(),
            }],
            ..Default::default()
        };
        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        register_interactive_themes(
            &Args::default(),
            &settings,
            &resources,
            &root.to_string_lossy(),
        );

        let info = crate::theme::available_themes_with_paths()
            .into_iter()
            .find(|info| info.name == name)
            .expect("extension theme is selectable");
        assert_eq!(info.source_info, Some(source_info));
        assert_eq!(
            info.source_path,
            Some(std::fs::canonicalize(&path).unwrap())
        );

        crate::theme::register_theme_paths(&[], std::path::Path::new("."));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fork_selector_lists_durable_user_messages_only() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-fork-selector-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        runtime
            .session
            .append_entry(
                EntryNoStats::Message {
                    id: "user-entry".to_string(),
                    message: pi_agent::agent::user_text_prompt(
                        "hello from the fork selector",
                        pi_ai::types::now_ms(),
                    ),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        runtime
            .session
            .append_entry(
                EntryNoStats::Custom {
                    id: "custom-entry".to_string(),
                    custom_type: "note".to_string(),
                    data: None,
                },
                "main",
            )
            .await
            .unwrap();

        let items = fork_selector_items(&runtime.session).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "user-entry");
        assert_eq!(items[0].label, "hello from the fork selector");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_fork_persists_pending_messages_and_returns_selected_text() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-fork-execute-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let original_session_id = runtime.session_id.clone();
        runtime
            .session
            .append_entry(
                EntryNoStats::Message {
                    id: "fork-target".to_string(),
                    message: pi_agent::agent::user_text_prompt(
                        "selected fork prompt",
                        pi_ai::types::now_ms(),
                    ),
                    terminate: None,
                },
                "main",
            )
            .await
            .unwrap();
        runtime.messages.push(pi_agent::agent::user_text_prompt(
            "pending before fork",
            pi_ai::types::now_ms(),
        ));
        let transcript = Arc::new(Mutex::new(Markdown::new(
            String::new(),
            1,
            0,
            it::tui_theme::markdown_theme(),
            None,
            None,
        )));
        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let result = execute_interactive_fork(
            &mut runtime,
            "fork",
            "fork-target".to_string(),
            ForkPosition::Before,
            InteractiveForkContext {
                settings: &settings,
                thinking_level: "off",
                transcript_md: &transcript,
                hide_thinking: true,
            },
            None,
        )
        .await;

        assert!(
            result.status.starts_with("fork session "),
            "{}",
            result.status
        );
        assert_eq!(result.editor_text.as_deref(), Some("selected fork prompt"));
        assert_eq!(runtime.persisted_until, runtime.messages.len());
        let source_metadata = runtime
            .repo
            .list(Some(&runtime.cwd))
            .await
            .unwrap()
            .into_iter()
            .find(|metadata| metadata.id == original_session_id)
            .expect("source session metadata should remain after fork");
        let source_session = runtime.repo.open(&source_metadata).await.unwrap();
        assert_eq!(
            source_session
                .find_entries(&pi_agent::session::state::EntryQuery {
                    order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(runtime
            .session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Serializes tests that mutate the process-global PATH /
    /// PI_SHARE_VIEWER_URL so parallel executions cannot race on the env.
    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Restores PATH / PI_SHARE_VIEWER_URL on drop. `replace_path` swaps PATH
    /// entirely (hermetic: no real `gh` visible); otherwise `bin_dir` is
    /// prepended so the fake `gh` shadows the real one.
    struct EnvGuard {
        old_path: String,
        old_viewer: Option<String>,
    }

    impl EnvGuard {
        fn install(bin_dir: &std::path::Path, viewer: &str) -> Self {
            let old_path = std::env::var("PATH").unwrap_or_default();
            let old_viewer = std::env::var("PI_SHARE_VIEWER_URL").ok();
            std::env::set_var("PATH", format!("{}:{}", bin_dir.display(), old_path));
            std::env::set_var("PI_SHARE_VIEWER_URL", viewer);
            EnvGuard {
                old_path,
                old_viewer,
            }
        }

        fn install_hermetic(bin_dir: &std::path::Path, viewer: &str) -> Self {
            let old_path = std::env::var("PATH").unwrap_or_default();
            let old_viewer = std::env::var("PI_SHARE_VIEWER_URL").ok();
            std::env::set_var("PATH", bin_dir.as_os_str());
            std::env::set_var("PI_SHARE_VIEWER_URL", viewer);
            EnvGuard {
                old_path,
                old_viewer,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::set_var("PATH", &self.old_path);
            match &self.old_viewer {
                Some(v) => std::env::set_var("PI_SHARE_VIEWER_URL", v),
                None => std::env::remove_var("PI_SHARE_VIEWER_URL"),
            }
        }
    }

    /// Build an InteractiveRuntime backed by a real session file in `root`.
    async fn test_runtime(root: &std::path::Path) -> InteractiveRuntime {
        let cwd = root.to_string_lossy().into_owned();
        let session_root = root.join("sessions");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut repo = JsonlSessionRepo::new(
            StdFileSystem::new(&cwd),
            session_root.to_string_lossy().into_owned(),
        );
        let session_id = pi_agent::session::new_id();
        let session = repo
            .create(CreateOptions {
                id: Some(session_id.clone()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .unwrap();
        let base_models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let faux_core = Some(crate::core::model_runtime::register_faux_provider(
            &base_models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        ));
        let model_registry = crate::core::model_registry::ModelRegistry::new(
            base_models,
            crate::core::model_config::ModelConfig::default(),
        );
        let models = model_registry.into_models();
        let model = faux_core
            .as_ref()
            .and_then(|core| core.models.first().cloned())
            .expect("faux model");
        let extensions = load_for_mode(
            &Args {
                no_extensions: true,
                ..Default::default()
            },
            &SettingsManager::in_memory(crate::core::settings::SettingsMap::new()),
            &cwd,
            &cwd,
            "interactive",
            true,
            None,
            "off",
        );
        let extension_resources = extensions.resources.clone();
        InteractiveRuntime {
            cwd: cwd.clone(),
            model_registry,
            models,
            faux_core,
            provider: "faux".to_string(),
            model,
            scoped_models: Vec::new(),
            messages: Vec::new(),
            session,
            repo,
            session_root: session_root.to_string_lossy().into_owned(),
            session_id,
            session_name: None,
            session_persistence: true,
            system_prompt: None,
            tools_enabled: true,
            builtin_tools_enabled: true,
            default_tool_names: None,
            native_provider_ids: Vec::new(),
            extensions,
            extension_resources,
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            extension_args: Args {
                no_extensions: true,
                ..Default::default()
            },
            extension_agent_dir: cwd.clone(),
            auto_resize_images: true,
            block_images: false,
            shell_command_prefix: None,
            shell_path: None,
            transport: "auto".to_string(),
            http_idle_timeout_ms: 300_000,
            provider_timeout_ms: None,
            provider_max_retries: None,
            max_retry_delay_ms: 60_000,
            websocket_connect_timeout_ms: None,
            retry_policy: pi_ai::utils::RetryPolicy {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 2_000,
            },
            compaction_settings: pi_agent::harness::compaction::DEFAULT_COMPACTION_SETTINGS,
            persisted_until: 0,
            active_tool_names: None,
            cache_entries: Vec::new(),
            interactive_harness: None,
            interactive_event_handler: None,
            interactive_tool_event_handler: None,
            extensions_shutdown: false,
        }
    }

    #[tokio::test]
    async fn interactive_bash_paths_apply_live_shell_settings() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-bash-settings-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        runtime.shell_command_prefix = Some("PI_BASH_PREFIX_VALUE=agent-turn".to_string());
        runtime.shell_path = Some("/bin/sh".to_string());

        let bash = interactive_turn_tools(&runtime)
            .into_iter()
            .find(|tool| tool.tool.name == "bash")
            .expect("interactive bash tool");
        let result = (bash.execute)(
            "configured-agent-bash".to_string(),
            serde_json::json!({"command": "printf %s \"$PI_BASH_PREFIX_VALUE\""}),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(result.content.iter().any(|content| {
            matches!(content, pi_ai::types::ContentBlock::Text { text, .. } if text == "agent-turn")
        }));

        let stream = Arc::new(Mutex::new(String::new()));
        let direct = start_interactive_bash_operation(
            "printf %s \"$PI_BASH_PREFIX_VALUE\"".to_string(),
            runtime.cwd.clone(),
            false,
            stream,
            Some("PI_BASH_PREFIX_VALUE=direct-turn".to_string()),
            Some("/bin/sh".to_string()),
        );
        let capture = direct.task.await.unwrap().unwrap();
        assert_eq!(capture.output, "direct-turn");

        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn interactive_models_json_reload_replaces_overlay_without_stale_state() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-model-reload-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        runtime
            .models
            .set_runtime_api_key("openai", "synthetic-runtime-key");

        let first = crate::core::model_config::ModelConfig::from_value(json!({
            "providers": {
                "reload-only": {
                    "baseUrl": "http://127.0.0.1:1/v1",
                    "api": "openai-completions",
                    "apiKey": "synthetic-overlay-key",
                    "models": [{"id": "before-reload", "name": "Before reload"}]
                }
            }
        }))
        .unwrap();
        assert!(apply_interactive_model_config(&mut runtime, first).is_empty());
        assert!(runtime
            .models
            .get_model("reload-only", "before-reload")
            .is_some());

        let second = crate::core::model_config::ModelConfig::from_value(json!({
            "providers": {
                "reload-only": {
                    "baseUrl": "http://127.0.0.1:2/v1",
                    "api": "openai-completions",
                    "apiKey": "synthetic-overlay-key",
                    "models": [{"id": "after-reload", "name": "After reload"}]
                }
            }
        }))
        .unwrap();
        assert!(apply_interactive_model_config(&mut runtime, second).is_empty());
        assert!(runtime
            .models
            .get_model("reload-only", "before-reload")
            .is_none());
        assert!(runtime
            .models
            .get_model("reload-only", "after-reload")
            .is_some());

        let malformed_path = root.join("models.json");
        std::fs::write(&malformed_path, "{\"providers\":").unwrap();
        let malformed = crate::core::model_config::ModelConfig::load(Some(&malformed_path));
        let notes = apply_interactive_model_config(&mut runtime, malformed);
        assert!(notes
            .iter()
            .any(|note| note.contains("models.json error: Failed to parse models.json")));
        assert!(runtime.models.get_provider("reload-only").is_none());
        assert_eq!(
            runtime
                .models
                .get_auth("openai", None)
                .and_then(|auth| auth.auth.api_key),
            Some("synthetic-runtime-key".to_string()),
            "runtime credentials must survive replacement facade composition"
        );
        assert!(runtime.models.get_provider("faux").is_some());

        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn live_bash_lifecycle_keeps_one_execution_block_and_measures_completion() {
        let options = it::messages::TranscriptRenderOptions {
            show_images: false,
            expand_tool_output: false,
            ..Default::default()
        };
        let mut live = InteractiveLiveTranscript::default();
        live.configure(options);
        live.on_tool_event(&RichAgentEvent::ToolExecutionStart {
            tool_call_id: "bash-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "printf PI_RUST_TUI_LIVE_TOOL"}),
        });
        live.on_tool_event(&RichAgentEvent::ToolExecutionUpdate {
            tool_call_id: "bash-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "printf PI_RUST_TUI_LIVE_TOOL"}),
            partial_result: serde_json::json!({
                "content": [{"type": "text", "text": "PI_RUST_TUI_LIVE_TOOL"}]
            }),
        });
        let running = live.render();
        assert!(running.contains("$ printf PI_RUST_TUI_LIVE_TOOL"));
        assert!(running.contains("PI_RUST_TUI_LIVE_TOOL"));
        assert!(running.contains("⏳ Running... (Esc to cancel)"));
        assert!(!running.contains("```json"));
        assert!(!running.contains("✓ **bash**"));

        live.on_tool_event(&RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "bash-1".to_string(),
            tool_name: "bash".to_string(),
            result: pi_agent::tools::AgentToolResult::output("PI_RUST_TUI_LIVE_TOOL"),
            is_error: false,
        });
        let completed = live.render();
        assert!(completed.contains("$ printf PI_RUST_TUI_LIVE_TOOL"));
        assert!(completed.contains("Took "));
        assert!(!completed.contains("Running..."));
        assert!(!completed.contains("✓ **bash**"));
    }

    #[test]
    fn live_tool_lifecycle_uses_compact_call_and_result_blocks() {
        let options = it::messages::TranscriptRenderOptions {
            show_images: false,
            expand_tool_output: false,
            ..Default::default()
        };
        let call = ContentBlock::tool_call(
            "call-1",
            "read",
            serde_json::json!({"path": "src/main.rs", "offset": 3, "limit": 2}),
        );
        let mut partial = AssistantMessage::new();
        partial.set_content(vec![call.clone()]);
        let event = AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: call,
            partial: partial.clone(),
        };

        let mut live = InteractiveLiveTranscript::default();
        live.configure(options);
        live.on_assistant_event(
            &event,
            it::messages::render_assistant_event_without_tool_calls_with_options(&event, options),
        );
        live.on_tool_event(&RichAgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "src/main.rs", "offset": 3, "limit": 2}),
        });

        let pending = live.render();
        assert!(pending.contains("**read**"));
        assert!(pending.contains("`src/main.rs`:3-4"));
        assert!(!pending.contains("\"path\": \"src/main.rs\""));
        assert!(!pending.contains("```json"));

        live.on_tool_event(&RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            result: pi_agent::tools::AgentToolResult::output("line one\nline two"),
            is_error: false,
        });
        let settled = live.render();
        assert!(settled.contains("✓ **read**"));
        assert!(!settled.contains("\"offset\": 3"));

        live.on_assistant_event(
            &AssistantMessageEvent::Start {
                partial: AssistantMessage::new(),
            },
            None,
        );
        let history = live.render();
        assert!(history.contains("✓ **read**"));
        assert!(history.contains("**read**"));
    }

    #[test]
    fn live_tool_call_is_not_suppressed_by_assistant_prose_and_keeps_turn_order() {
        let options = it::messages::TranscriptRenderOptions {
            show_images: false,
            expand_tool_output: false,
            ..Default::default()
        };
        let mut assistant = AssistantMessage::new();
        assistant.set_content(vec![ContentBlock::text("I will read the file now.")]);
        let text_event = AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: "I will read the file now.".to_string(),
            partial: assistant,
        };

        let mut live = InteractiveLiveTranscript::default();
        live.configure(options);
        live.on_assistant_event(
            &text_event,
            it::messages::render_assistant_event_without_tool_calls_with_options(
                &text_event,
                options,
            ),
        );
        live.on_tool_event(&RichAgentEvent::ToolExecutionStart {
            tool_call_id: "read-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "src/main.rs"}),
        });
        live.on_tool_event(&RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "read-1".to_string(),
            tool_name: "read".to_string(),
            result: pi_agent::tools::AgentToolResult::output("fn main() {}"),
            is_error: false,
        });

        let mut next_assistant = AssistantMessage::new();
        next_assistant.set_content(vec![ContentBlock::text("The file is small.")]);
        let next_event = AssistantMessageEvent::Start {
            partial: next_assistant,
        };
        live.on_assistant_event(&next_event, None);

        let transcript = live.render();
        let prose = transcript
            .find("I will read the file now.")
            .expect("assistant prose should be retained");
        let call = transcript
            .find("⏳ **read**")
            .expect("tool call should be rendered");
        let result = transcript
            .find("✓ **read**")
            .expect("tool result should be rendered");
        assert!(prose < call, "assistant prose must precede its tool call");
        assert!(call < result, "tool call must precede its result");
        assert!(!transcript.contains("\"path\": \"src/main.rs\""));
    }

    #[test]
    fn live_tool_projection_preserves_prose_after_an_interleaved_tool_call() {
        let options = it::messages::TranscriptRenderOptions {
            show_images: false,
            expand_tool_output: false,
            ..Default::default()
        };
        let call = ContentBlock::tool_call(
            "read-after-1",
            "read",
            serde_json::json!({"path": "src/lib.rs"}),
        );
        let mut partial = AssistantMessage::new();
        partial.set_content(vec![
            ContentBlock::text("Before the read."),
            call.clone(),
            ContentBlock::text("After the read call, I will summarize it."),
        ]);
        let event = AssistantMessageEvent::ToolCallEnd {
            content_index: 1,
            tool_call: call,
            partial,
        };

        let mut live = InteractiveLiveTranscript::default();
        live.configure(options);
        live.on_assistant_event(&event, None);
        live.on_tool_event(&RichAgentEvent::ToolExecutionStart {
            tool_call_id: "read-after-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "src/lib.rs"}),
        });
        live.on_tool_event(&RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "read-after-1".to_string(),
            tool_name: "read".to_string(),
            result: pi_agent::tools::AgentToolResult::output("pub fn answer() {}"),
            is_error: false,
        });

        let transcript = live.render();
        let before = transcript
            .find("Before the read.")
            .expect("leading assistant prose should be retained");
        let call = transcript
            .find("⏳ **read**")
            .expect("interleaved tool call should be rendered");
        let result = transcript
            .find("✓ **read**")
            .expect("interleaved tool result should be rendered");
        let after = transcript
            .find("After the read call")
            .expect("assistant prose after the tool call should be retained");
        assert!(before < call, "leading prose must precede the tool call");
        assert!(call < result, "tool call must precede its result");
        assert!(result < after, "tool result must precede trailing prose");
        assert!(!transcript.contains("\"path\": \"src/lib.rs\""));
        assert!(!transcript.contains("```json"));
    }

    #[test]
    fn user_transcript_component_emits_osc133_zone_boundaries() {
        let component = TranscriptVisualComponent::new(
            TranscriptVisualKind::User,
            "first prompt line\nsecond prompt line".to_string(),
            1,
        );
        let lines = component.render(40);
        let first = lines.first().expect("user component should render content");
        assert!(first.starts_with(OSC133_ZONE_START));
        assert_eq!(pi_tui::utils::visible_width(first), 40);
        let visible_first = pi_tui::strip_terminal_sequences(first);
        assert!(visible_first.contains("first prompt line"));
        assert!(lines.last().is_some_and(|line| {
            line.contains(OSC133_ZONE_END) && line.contains(OSC133_ZONE_FINAL)
        }));
    }

    #[test]
    fn live_tool_failure_is_terminal_and_keeps_compact_error_row() {
        let options = it::messages::TranscriptRenderOptions {
            show_images: false,
            expand_tool_output: false,
            ..Default::default()
        };
        let mut live = InteractiveLiveTranscript::default();
        live.configure(options);
        live.on_tool_event(&RichAgentEvent::ToolExecutionStart {
            tool_call_id: "grep-1".to_string(),
            tool_name: "grep".to_string(),
            args: serde_json::json!({"pattern": "missing", "path": "."}),
        });
        live.on_tool_event(&RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "grep-1".to_string(),
            tool_name: "grep".to_string(),
            result: pi_agent::tools::AgentToolResult::output("Operation aborted"),
            is_error: true,
        });

        let transcript = live.render();
        assert!(transcript.contains("✗ **grep**"));
        assert!(transcript.contains("Operation aborted"));
        let call = transcript
            .find("⏳ **grep**")
            .expect("the compact call row should remain in the settled tool block");
        let failure = transcript
            .find("✗ **grep**")
            .expect("the failed result row should be terminal");
        assert!(call < failure);
        assert!(!transcript.contains("```json"));
    }

    #[tokio::test]
    async fn interactive_stream_turn_uses_harness_transcript_and_events() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-harness-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let deltas = Arc::new(Mutex::new(Vec::<String>::new()));
        let deltas_for_event = deltas.clone();
        let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = Arc::new(move |event| {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                deltas_for_event
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(delta.clone());
            }
        });

        let new_messages = stream_turn(&mut runtime, "hello".to_string(), on_event)
            .await
            .unwrap();

        assert_eq!(new_messages.len(), 2, "prompt plus assistant response");
        assert_eq!(runtime.messages.len(), 2);
        assert!(runtime.messages.iter().any(|message| {
            matches!(
                message,
                pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(assistant))
                    if assistant.content().iter().any(|block| matches!(
                        block,
                        pi_ai::types::ContentBlock::Text { text, .. }
                            if text.contains("faux response to: hello")
                    ))
            )
        }));
        assert!(!deltas
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty());
        let entries = runtime
            .session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 2, "a completed turn is durable immediately");
        assert_eq!(runtime.persisted_until, runtime.messages.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_turn_reuses_one_harness_across_turns_and_persists_both() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-persistent-harness-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = Arc::new(|_| {});

        stream_turn(&mut runtime, "first".to_string(), on_event.clone())
            .await
            .unwrap();
        let first_harness = runtime
            .interactive_harness
            .as_ref()
            .map(Arc::as_ptr)
            .expect("first turn installs a live harness");

        stream_turn(&mut runtime, "second".to_string(), on_event)
            .await
            .unwrap();
        let second_harness = runtime
            .interactive_harness
            .as_ref()
            .map(Arc::as_ptr)
            .expect("second turn keeps the live harness");
        assert_eq!(first_harness, second_harness);
        assert_eq!(runtime.messages.len(), 4, "two prompts and two responses");

        let entries = runtime
            .session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 4, "both turns are durable exactly once");
        let messages = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message { message, .. } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages
                .iter()
                .filter_map(|message| match message {
                    pi_agent::types::AgentMessage::Core(Message::User(user)) => {
                        Some(pi_agent::agent::user_content_text(user))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_turn_tools_reject_filesystem_extensions_in_zero_js_build() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-extension-policy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let args = Args {
            extensions: vec![root
                .join("filesystem-extension")
                .to_string_lossy()
                .into_owned()],
            no_extensions: true,
            ..Default::default()
        };
        let loaded = load_for_mode(
            &args,
            &SettingsManager::in_memory(crate::core::settings::SettingsMap::new()),
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "interactive",
            true,
            None,
            "off",
        );
        assert!(loaded.runner.extensions().is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].error.contains("Rust-native-only"));

        let mut runtime = test_runtime(&root).await;
        runtime.extensions = loaded;
        runtime.builtin_tools_enabled = false;
        assert!(interactive_turn_tools(&runtime).is_empty());
        assert_eq!(runtime.extensions.host.snapshot()["activeTools"], json!([]));

        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_turn_tools_apply_allowlist_exclusions_and_defaults() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-tool-policy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let mut runtime = test_runtime(&root).await;
        runtime.extension_args.tools = Some(vec!["read".to_owned(), "grep".to_owned()]);
        let selected = interactive_turn_tools(&runtime);
        assert_eq!(
            selected
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "grep"]
        );

        runtime.extension_args.no_tools = true;
        let selected = interactive_turn_tools(&runtime);
        assert_eq!(
            selected
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "grep"],
            "an explicit allowlist overrides --no-tools"
        );

        runtime.extension_args.tools = None;
        runtime.extension_args.no_tools = false;
        runtime.extension_args.exclude_tools = Some(vec!["bash".to_owned(), "write".to_owned()]);
        let selected = interactive_turn_tools(&runtime);
        assert_eq!(
            selected
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "edit"]
        );

        runtime.extension_args.exclude_tools = None;
        runtime.default_tool_names = Some(vec!["grep".to_owned()]);
        let selected = interactive_turn_tools(&runtime);
        assert_eq!(
            selected
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["grep"]
        );

        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn interactive_reload_keeps_automatic_filesystem_discovery_empty() {
        let root = std::env::temp_dir().join(format!(
            "pi-interactive-extension-reload-policy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join(".pi/extensions")).unwrap();
        let args = Args {
            no_extensions: false,
            ..Default::default()
        };
        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let mut runtime = test_runtime(&root).await;
        runtime.extension_args = args.clone();
        runtime.extension_agent_dir = root.to_string_lossy().into_owned();
        runtime.extensions = load_for_mode(
            &args,
            &settings,
            &runtime.cwd,
            &runtime.extension_agent_dir,
            "interactive",
            true,
            runtime.session_name.clone(),
            "off",
        );
        assert!(runtime.extensions.errors.is_empty());
        runtime.builtin_tools_enabled = false;
        assert!(interactive_turn_tools(&runtime).is_empty());

        let notes = reload_extensions(&mut runtime, &settings, "off");
        assert!(notes.is_empty(), "reload notes: {notes:?}");
        assert!(runtime.extensions.errors.is_empty());
        assert!(interactive_turn_tools(&runtime).is_empty());
        assert!(runtime.extensions.runner.get_extension_paths().is_empty());

        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Write a fake `gh` script into `bin_dir`. `auth_status` is the exit code
    /// for `gh auth status`; `gist_url` is the stdout for `gh gist create`
    /// (None => exit 1).
    fn install_fake_gh(bin_dir: &std::path::Path, auth_status: i32, gist_url: Option<&str>) {
        std::fs::create_dir_all(bin_dir).unwrap();
        let script = match gist_url {
            Some(url) => format!(
                "#!/bin/sh\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then exit {auth_status}; fi\nif [ \"$1\" = \"gist\" ] && [ \"$2\" = \"create\" ]; then echo '{url}'; exit 0; fi\nexit 1\n"
            ),
            None => format!(
                "#!/bin/sh\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"status\" ]; then exit {auth_status}; fi\nexit 1\n"
            ),
        };
        std::fs::write(bin_dir.join("gh"), script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin_dir.join("gh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn share_creates_secret_gist_and_prints_viewer_url() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        install_fake_gh(
            &root.join("bin"),
            0,
            Some("https://gist.github.com/fakeuser/abc123"),
        );
        let _guard = EnvGuard::install(&root.join("bin"), "https://pi.dev/session/");
        let msg = run_share(&runtime, false)
            .await
            .expect("share should succeed");
        assert_eq!(
            msg,
            "Share URL: https://pi.dev/session/#abc123\nGist: https://gist.github.com/fakeuser/abc123"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_requires_gh_auth() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        install_fake_gh(&root.join("bin"), 1, None);
        let _guard = EnvGuard::install(&root.join("bin"), "https://pi.dev/session/");
        let err = run_share(&runtime, false).await.unwrap_err();
        assert_eq!(
            err,
            "GitHub CLI is not logged in. Run 'gh auth login' first."
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_reports_missing_gh() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        // PATH pointing at an empty dir only: no gh binary anywhere.
        let empty = root.join("empty-bin");
        std::fs::create_dir_all(&empty).unwrap();
        let _guard = EnvGuard::install_hermetic(&empty, "https://pi.dev/session/");
        let err = run_share(&runtime, false).await.unwrap_err();
        assert_eq!(
            err,
            "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_dry_run_skips_gh() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock().lock().await;
        let runtime = test_runtime(&root).await;
        let msg = run_share(&runtime, true).await.unwrap();
        assert_eq!(msg, "PI_SHARE_DRY_RUN=1: /share skipped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_share_viewer_environment_uses_upstream_default() {
        assert_eq!(share_viewer_url(None), "https://pi.dev/session/");
        assert_eq!(share_viewer_url(Some("")), "https://pi.dev/session/");
        assert_eq!(
            share_viewer_url(Some(" https://viewer.example/ ")),
            " https://viewer.example/ "
        );
    }

    #[tokio::test]
    async fn auto_compact_replaces_context_when_over_threshold() {
        let _env = env_lock().lock().await;
        let root = std::env::temp_dir().join(format!("pi-compact-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        // Register faux in the models facade so complete_simple resolves
        // (mirrors RpcRuntime::new's scripted faux registration).
        {
            use pi_ai::models::{
                create_provider, CreateProviderOptions, ProviderApiSpec, ProviderStreams,
            };
            use pi_ai::providers::{
                faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
                RegisterFauxProviderOptions,
            };
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(
                    "Compaction summary: history retained",
                )],
                FauxAssistantOptions::default(),
            ))]);
            let stream_core = core.clone();
            let stream = Arc::new(
                move |model: &pi_ai::model::Model,
                      ctx: &pi_ai::types::Context,
                      _options: Option<&pi_ai::types::StreamOptions>| {
                    stream_core.stream(model, ctx, None)
                },
            );
            let simple_core = core.clone();
            let stream_simple = Arc::new(
                move |model: &pi_ai::model::Model,
                      ctx: &pi_ai::types::Context,
                      options: Option<&pi_ai::types::SimpleStreamOptions>| {
                    simple_core.stream(model, ctx, options)
                },
            );
            runtime
                .models
                .set_provider(create_provider(CreateProviderOptions {
                    id: "faux".to_string(),
                    name: Some("Faux".to_string()),
                    base_url: None,
                    headers: None,
                    auth: pi_ai::auth::ProviderAuth {
                        api_key: Some(pi_ai::auth::env_api_key_auth(
                            "Faux API key",
                            vec!["FAUX_API_KEY"],
                        )),
                        oauth: None,
                    },
                    models: core.models.clone(),
                    api: ProviderApiSpec::Single(ProviderStreams {
                        stream,
                        stream_simple,
                        fetch_deferred: None,
                        cancel_deferred: None,
                    }),
                    filter_models: None,
                }));
        }
        // The env-key auth resolves when FAUX_API_KEY is set.
        std::env::set_var("FAUX_API_KEY", "test");
        // Tiny context window so the threshold triggers immediately.
        runtime.model.context_window = 1000;
        // A few long messages push the estimate over window - reserve.
        for i in 0..8 {
            let text = format!("message {i}: {}", "x".repeat(400));
            runtime.messages.push(pi_agent::agent::user_text_prompt(
                text,
                pi_ai::types::now_ms(),
            ));
        }
        // prepare_compaction reads session entries, so persist the messages.
        persist_messages(&mut runtime.session, &runtime.messages).await;
        runtime.persisted_until = runtime.messages.len();
        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let (enabled, reserve_tokens, keep_recent_tokens) = settings.get_compaction_settings();
        let compaction_settings = pi_agent::harness::compaction::CompactionSettings {
            enabled,
            reserve_tokens,
            keep_recent_tokens,
        };
        let estimate = pi_agent::harness::compaction::estimate_context_tokens(&runtime.messages);
        assert!(
            pi_agent::harness::compaction::should_compact(
                estimate.tokens,
                runtime.model.context_window,
                &compaction_settings
            ),
            "test setup: context should be over threshold (tokens={})",
            estimate.tokens
        );
        let compacted = maybe_auto_compact(&mut runtime, &settings)
            .await
            .expect("auto-compact");
        assert!(compacted, "compaction should have run");
        // The context is now the summary message + retained tail.
        assert!(!runtime.messages.is_empty(), "context replaced");
        let first = &runtime.messages[0];
        let text: String = match first {
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::User(u)) => {
                match u.content() {
                    pi_ai::types::UserContentBody::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|b| match b {
                            pi_ai::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect(),
                    pi_ai::types::UserContentBody::String(s) => s.clone(),
                }
            }
            _ => String::new(),
        };
        assert!(
            text.contains("Compaction summary"),
            "summary message: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn interactive_compaction_settings_follow_settings_manager() {
        let mut settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let defaults = interactive_compaction_settings(&settings);
        assert!(defaults.enabled);
        assert_eq!(defaults.reserve_tokens, 16_384);
        assert_eq!(defaults.keep_recent_tokens, 20_000);

        settings.set_compaction_enabled(false);
        let disabled = interactive_compaction_settings(&settings);
        assert!(!disabled.enabled);
        assert_eq!(disabled.reserve_tokens, defaults.reserve_tokens);
        assert_eq!(disabled.keep_recent_tokens, defaults.keep_recent_tokens);
    }

    #[test]
    fn direct_thinking_selection_preserves_hide_thinking_setting() {
        let mut settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        settings.set_hide_thinking_block(true);
        assert!(hide_thinking_for_level(&settings, "medium"));
        assert!(hide_thinking_for_level(&settings, "off"));

        settings.set_hide_thinking_block(false);
        assert!(!hide_thinking_for_level(&settings, "medium"));
        assert!(hide_thinking_for_level(&settings, "off"));
    }

    #[tokio::test]
    async fn auto_compact_skips_when_under_threshold() {
        let _env = env_lock().lock().await;
        let root = std::env::temp_dir().join(format!("pi-compact-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        runtime.model.context_window = 1_000_000;
        runtime.messages.push(pi_agent::agent::user_text_prompt(
            "hi".to_string(),
            pi_ai::types::now_ms(),
        ));
        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let compacted = maybe_auto_compact(&mut runtime, &settings)
            .await
            .expect("auto-compact");
        assert!(!compacted, "no compaction under threshold");
        assert_eq!(runtime.messages.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn manual_compact_is_a_noop_without_session_history() {
        let _env = env_lock().lock().await;
        let root = std::env::temp_dir().join(format!("pi-compact-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;

        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        let compacted =
            compact_interactive(&mut runtime, &settings, Some("Focus on decisions"), true)
                .await
                .expect("manual compact");

        assert!(!compacted);
        assert!(runtime.messages.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn manual_compact_uses_extension_result_and_persists_once() {
        use crate::core::extensions::types::{Extension, HandlerFn};
        use crate::core::extensions::{ExtensionRunner, ExtensionRuntime};

        let root =
            std::env::temp_dir().join(format!("pi-compact-extension-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut runtime = test_runtime(&root).await;
        runtime.messages = (0..6)
            .map(|index| {
                pi_agent::agent::user_text_prompt(
                    format!("message {index}: {}", "x".repeat(200)),
                    pi_ai::types::now_ms(),
                )
            })
            .collect();
        persist_messages(&mut runtime.session, &runtime.messages).await;
        runtime.persisted_until = runtime.messages.len();

        let observed = Arc::new(Mutex::new(None::<Value>));
        let handler: HandlerFn = {
            let observed = Arc::clone(&observed);
            Arc::new(move |_, event| {
                *observed.lock().unwrap_or_else(|error| error.into_inner()) = Some(event.clone());
                let first_kept_entry_id = event["branchEntries"]
                    .as_array()
                    .and_then(|entries| entries.last())
                    .and_then(|entry| entry["id"].as_str())
                    .expect("last branch entry id");
                Ok(Some(json!({
                    "compaction": {
                        "summary": "summary supplied by extension",
                        "firstKeptEntryId": first_kept_entry_id,
                        "tokensBefore": 321,
                        "details": {
                            "readFiles": ["README.md"],
                            "modifiedFiles": ["src/lib.rs"]
                        }
                    }
                })))
            })
        };
        let mut extension = Extension {
            path: "compact-extension".to_string(),
            ..Default::default()
        };
        extension
            .handlers
            .insert("session_before_compact".to_string(), vec![handler]);
        let extension_runtime = Arc::new(Mutex::new(ExtensionRuntime::new()));
        runtime.extensions.runner = Arc::new(ExtensionRunner::new(
            vec![extension],
            Arc::clone(&extension_runtime),
            runtime.cwd.clone(),
        ));
        runtime.extensions.runtime = extension_runtime;

        let settings = SettingsManager::in_memory(crate::core::settings::SettingsMap::new());
        assert!(
            compact_interactive(&mut runtime, &settings, Some("focus on decisions"), true,)
                .await
                .expect("extension compaction")
        );

        let entries = runtime
            .session
            .find_entries(&pi_agent::session::state::EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        let compacted = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Compaction {
                    summary,
                    retained_tail,
                    tokens_before,
                    details,
                    ..
                } => Some((summary, retained_tail, tokens_before, details)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            compacted.len(),
            1,
            "custom result must persist exactly once"
        );
        assert_eq!(compacted[0].0, "summary supplied by extension");
        assert_eq!(*compacted[0].2, 321);
        assert_eq!(compacted[0].1.len(), 1);
        assert_eq!(
            compacted[0].3.as_ref().unwrap()["readFiles"],
            json!(["README.md"])
        );
        let event = observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .expect("hook payload");
        assert_eq!(event["reason"], "manual");
        assert_eq!(event["willRetry"], false);
        assert_eq!(event["customInstructions"], "focus on decisions");
        assert!(event["preparation"]["messagesToSummarize"].is_array());
        assert!(event["branchEntries"].is_array());

        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn footer_usage_aggregates_assistant_messages_and_hit_rate() {
        use pi_ai::types::{Cost, Message, Usage};
        let usage = |input: i64, cache_read: i64, output: i64| Usage {
            input,
            output,
            cache_read,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output + cache_read,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.01,
            },
        };
        let with_usage = |u: Usage| -> pi_agent::types::AgentMessage {
            let mut msg = pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text("hi")],
                pi_ai::providers::FauxAssistantOptions::default(),
            );
            msg.set_usage(u);
            pi_agent::types::AgentMessage::Core(Message::Assistant(msg))
        };

        let messages = vec![
            with_usage(usage(100, 50, 30)),
            with_usage(usage(200, 50, 70)),
        ];
        let (totals, hit_rate) = footer_usage_from_messages(&messages);
        let totals = totals.expect("usage present");
        assert_eq!(totals.input, 300);
        assert_eq!(totals.output, 100);
        assert_eq!(totals.cache_read, 100);
        // Last turn: 200 prompt, 50 cached => 50 / 250 = 20%.
        assert!((hit_rate.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn footer_usage_empty_when_no_assistant_usage() {
        let messages = vec![pi_agent::agent::user_text_prompt(
            "hi".to_string(),
            pi_ai::types::now_ms(),
        )];
        let (totals, hit_rate) = footer_usage_from_messages(&messages);
        assert!(totals.is_none());
        assert!(hit_rate.is_none());
    }

    #[test]
    fn cache_notice_is_rederived_with_idle_label_and_threshold() {
        let entries = vec![
            json!({
                "type": "message",
                "timestamp": 1_000,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude",
                    "usage": {
                        "input": 0,
                        "output": 1,
                        "cacheRead": 0,
                        "cacheWrite": 25_000,
                        "totalTokens": 25_001,
                        "cost": {
                            "input": 0.0,
                            "output": 0.01,
                            "cache_read": 0.0,
                            "cache_write": 100.0,
                            "total": 100.01
                        }
                    }
                }
            }),
            json!({
                "type": "message",
                "timestamp": 301_001,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude",
                    "usage": {
                        "input": 24_000,
                        "output": 1,
                        "cacheRead": 1_000,
                        "cacheWrite": 0,
                        "totalTokens": 25_001,
                        "cost": {
                            "input": 72_000.0,
                            "output": 0.01,
                            "cache_read": 300.0,
                            "cache_write": 0.0,
                            "total": 72_300.01
                        }
                    }
                }
            }),
        ];

        let notices = cache_notice_timestamps(&entries);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, 301_001);
        assert!(notices[0].1.contains("Cache miss after 5m idle"));
        assert!(notices[0].1.contains("24k tokens re-billed"));
    }

    #[test]
    fn footer_entries_include_summary_usage_and_cache_rebilling_line() {
        let entries = vec![
            json!({
                "type": "message",
                "timestamp": 1,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude",
                    "usage": {
                        "input": 5_000,
                        "output": 10,
                        "cacheRead": 0,
                        "cacheWrite": 5_000,
                        "totalTokens": 10_010,
                        "cost": {"input": 1.0, "output": 0.1, "cache_read": 0.0, "cache_write": 2.0, "total": 3.1}
                    }
                }
            }),
            json!({
                "type": "compaction",
                "timestamp": 2,
                "usage": {
                    "input": 100,
                    "output": 20,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 120,
                    "cost": {"input": 0.2, "output": 0.1, "cache_read": 0.0, "cache_write": 0.0, "total": 0.3}
                }
            }),
        ];
        let (usage, _) = footer_usage_from_entries(&entries);
        let usage = usage.expect("usage");
        assert_eq!(usage.input, 5_100);
        assert_eq!(usage.output, 30);
        assert!((usage.cost - 3.4).abs() < 1e-9);
        let waste = crate::core::cache_stats::compute_cache_waste(
            &entries,
            &crate::core::cache_stats::NoPrices,
        );
        assert_eq!(format_cache_waste_line(waste), None);
        assert_eq!(
            format_cache_waste_line(crate::core::cache_stats::CacheWasteTotals {
                missed_tokens: 24_000,
                missed_cost: 0.25,
                miss_count: 2,
            }),
            Some("Cache Re-billed: $0.250 (24000 tokens, 2 misses)".to_string())
        );
    }

    #[test]
    fn transcript_reinjects_cache_notice_after_matching_assistant() {
        let mut assistant = pi_ai::providers::faux_assistant_message(
            vec![pi_ai::types::ContentBlock::text("answer")],
            pi_ai::providers::FauxAssistantOptions::default(),
        );
        assistant = assistant.with_timestamp(42);
        let messages = vec![pi_agent::types::AgentMessage::Core(Message::Assistant(
            assistant,
        ))];
        let transcript = it::compose_transcript_with_cache_notices(
            &messages,
            false,
            "",
            &[(42, "⚠ Cache miss: 24k tokens re-billed".to_string())],
        );
        assert!(transcript.contains("answer"));
        assert!(transcript.contains("> ⚠ Cache miss: 24k tokens re-billed"));
    }

    #[test]
    fn fullscreen_scrollbar_setting_applies_only_to_fullscreen_and_defaults_to_auto() {
        assert_eq!(
            fullscreen_scrollbar_mode("always", true),
            pi_tui::ScrollbarMode::Always
        );
        assert_eq!(
            fullscreen_scrollbar_mode("hidden", true),
            pi_tui::ScrollbarMode::Hidden
        );
        assert_eq!(
            fullscreen_scrollbar_mode("unknown", true),
            pi_tui::ScrollbarMode::Auto
        );
        assert_eq!(
            fullscreen_scrollbar_mode("always", false),
            pi_tui::ScrollbarMode::Hidden
        );
    }
}
