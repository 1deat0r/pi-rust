//! Interactive TUI mode — port of `packages/coding-agent/src/modes/interactive/
//! interactive-mode.ts` using the ported pi-tui component surface.
//!
//! Drives the Editor (multi-line, history, undo, autocomplete), the Markdown
//! transcript, slash-command dispatch with model/thinking/theme/settings
//! selectors, a footer, and the agent turn loop.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::session::Session as JsonlSession;
use pi_agent::session::state::{ForkOptions, ForkPosition};
use pi_agent::session::types::EntryNoStats;
use pi_agent::session::JsonlSessionRepo;
use pi_ai::auth::AuthInteraction;
use pi_ai::model::Model;
use pi_ai::types::{AssistantMessageEvent, Message};
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
use crate::interactive::footer::{self, FooterData};
use crate::interactive::selectors::ListSelector;
use crate::interactive::settings_panel::SettingsPanel;
use crate::interactive::slash::SlashKind;
use crate::interactive::{Modal, SubmitAction};

use pi_tui::components::select_list::SelectItem;
use pi_tui::components::{Editor, Markdown, Text};
use pi_tui::keys::{parse_key, TuiKey};

use pi_tui::terminal::TerminalBackend;
use pi_tui::tui::{Component, SharedComponent, Tree};

/// Interactive session runtime (reuses the run/RPC wiring).
struct InteractiveRuntime {
    cwd: String,
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
    extensions: LoadedExtensions,
    extension_resources: ResourceDiscovery,
    prompt_templates: Vec<crate::core::prompt_templates::PromptTemplate>,
    native_provider_ids: Vec<String>,
    extension_args: Args,
    extension_agent_dir: String,
    auto_resize_images: bool,
    block_images: bool,
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
}

impl Drop for InteractiveRuntime {
    fn drop(&mut self) {
        let _ = self.extensions.runner.emit_session_shutdown("quit");
        self.extensions
            .runner
            .invalidate(Some("interactive mode shutdown"));
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
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl InteractiveInputReader {
    fn start(terminal: Arc<Mutex<TerminalBackend>>) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = Arc::clone(&stop);
        let task_terminal = Arc::clone(&terminal);
        let task = tokio::task::spawn_blocking(move || {
            while !task_stop.load(Ordering::Acquire) {
                let event = match task_terminal.lock() {
                    Ok(mut terminal) => terminal
                        .next_event()
                        .map_err(|error| format!("read terminal input: {error}")),
                    Err(_) => Err("terminal lock poisoned".to_string()),
                };

                match event {
                    Ok(event) => {
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
                        // input. Do not flood the async queue with them or
                        // immediately reacquire the terminal mutex in a
                        // tight loop and starve the renderer.
                        if matches!(
                            &event,
                            pi_tui::terminal::TerminalEvent::Key(key) if key.is_empty()
                        ) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                        if sender.send(Ok(event)).is_err() {
                            break;
                        }
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
            stop,
            task: Some(task),
        }
    }

    async fn recv(&mut self) -> Option<Result<pi_tui::terminal::TerminalEvent, String>> {
        self.receiver.recv().await
    }

    fn pending_cancel(&mut self) -> bool {
        while let Ok(event) = self.receiver.try_recv() {
            let Ok(pi_tui::terminal::TerminalEvent::Key(raw)) = event else {
                continue;
            };
            let key = parse_key(&raw);
            if key.base == "esc" || key.base == "escape" || (key.ctrl && key.base == "c") {
                return true;
            }
        }
        false
    }

    async fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    async fn restart(&mut self) {
        self.stop_worker().await;
        let replacement = Self::start(self.terminal.clone());
        *self = replacement;
    }

    async fn shutdown(mut self) {
        self.stop_worker().await;
    }
}

fn should_exit_on_key(key: &TuiKey, editor_text: &str) -> bool {
    key.ctrl && key.base == "d" && editor_text.is_empty()
}

fn resumable_sessions(
    sessions: Vec<pi_agent::session::types::SessionMetadata>,
    current_id: &str,
) -> Vec<pi_agent::session::types::SessionMetadata> {
    sessions
        .into_iter()
        .filter(|session| session.id != current_id)
        .collect()
}

/// Build the tools for one interactive turn and refresh the extension host
/// catalog from the exact set that is available to that turn.
fn interactive_turn_tools(runtime: &InteractiveRuntime) -> Vec<pi_agent::tools::AgentTool> {
    let mut tools = if runtime.tools_enabled && runtime.builtin_tools_enabled {
        vec![
            pi_agent::tools::bash_tool(runtime.cwd.clone()),
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
    if let Some(active_tool_names) = &runtime.active_tool_names {
        tools.retain(|tool| active_tool_names.iter().any(|name| name == &tool.tool.name));
    }
    tools
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
    let mut sections = Vec::new();
    if let Some(prompt) = args.system_prompt.as_deref() {
        if !prompt.trim().is_empty() {
            sections.push(prompt.trim().to_string());
        }
    }
    let skills = crate::run::build_skills_block(
        args,
        cwd,
        std::path::Path::new(agent_dir),
        settings,
        resources,
    );
    if !skills.is_empty() {
        sections.push(skills);
    }
    (!sections.is_empty()).then(|| sections.join("\n"))
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

    runtime.session = session;
    runtime.session_id = new_id;
    runtime.session_name = runtime.session.get_name().await;
    runtime.messages = messages;
    runtime.cache_entries = cache_entries;
    runtime.persisted_until = runtime.messages.len();
    context
        .transcript_md
        .lock()
        .unwrap()
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
                    runtime.session = session;
                    runtime.session_id = new_id;
                    runtime.session_name = None;
                    runtime.messages.clear();
                    runtime.cache_entries.clear();
                    runtime.persisted_until = 0;
                    transcript_md.lock().unwrap().set_text("");
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
                    let metadata = crate::run::resolve_session_metadata(&runtime.repo, selector)
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
                    register_interactive_themes(
                        &runtime.extension_args,
                        settings,
                        &runtime.extension_resources,
                        &runtime.cwd,
                    );
                    if let Some(theme_name) = settings.get_theme_setting() {
                        load_interactive_theme(theme_name);
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
                runtime.provider = provider.to_string();
                runtime.model = model.clone();
                runtime.extensions.host.set_model(Some(model_value));
            } else {
                tracing::warn!(%provider, %model_id, "extension requested an unavailable interactive model");
            }
        }
    }
    if let Some(active_tools) = changes.active_tools {
        runtime.active_tool_names = Some(active_tools);
    }
}

/// Cycle the active model through the explicit scoped-models set. Pi uses
/// Ctrl+P for this operation; an empty set intentionally leaves the normal
/// model selector behavior unchanged.
fn cycle_scoped_model(
    runtime: &mut InteractiveRuntime,
    settings: &mut SettingsManager,
) -> Option<String> {
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
    runtime.provider = provider.to_string();
    runtime.model = model;
    let _ = it::apply_model_selection(settings, &next_reference);
    Some(format!("Model: {next_reference}"))
}

fn apply_model_reference(
    runtime: &mut InteractiveRuntime,
    settings: &mut SettingsManager,
    value: &str,
) -> Result<String, String> {
    let (provider, model_id) = value
        .trim()
        .split_once('/')
        .filter(|(provider, model_id)| !provider.is_empty() && !model_id.is_empty())
        .ok_or_else(|| "usage: /model <provider/model>".to_string())?;
    let model = runtime
        .models
        .get_model(provider, model_id)
        .ok_or_else(|| format!("model not found: {value}"))?;
    runtime.provider = provider.to_string();
    runtime.model = model;
    settings.set_default_model_and_provider(provider.to_string(), model_id.to_string());
    Ok(format!(
        "Model: {}/{}",
        runtime.provider, runtime.model.name
    ))
}

fn maybe_add_daxnuts_component(
    runtime: &InteractiveRuntime,
    easter_egg_components: &mut Vec<SharedComponent>,
    animation_until: &mut Option<std::time::Instant>,
) {
    if it::easter_eggs::is_daxnuts_model(&runtime.provider, &runtime.model.id) {
        easter_egg_components.push(it::easter_eggs::daxnuts_component());
        *animation_until = Some(std::time::Instant::now() + it::easter_eggs::animation_duration());
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
    let mut lines = transcript_md.lock().unwrap().render(width);
    for component in easter_egg_components {
        lines.extend(component.lock().unwrap().render(width));
    }
    lines.extend(editor.lock().unwrap().render(width));
    lines.extend(footer_text.lock().unwrap().render(width));
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

/// Stream a prompt through the agent loop, observing raw events.
struct InteractiveTurnWorker {
    agent: Arc<pi_agent::rich_agent::Agent>,
    task: tokio::task::JoinHandle<Result<Vec<pi_agent::types::AgentMessage>, String>>,
    _idle_guard: InteractiveIdleGuard,
}

async fn start_interactive_turn(
    runtime: &mut InteractiveRuntime,
    message: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
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
    let api_key = std::env::var(config::ENV_KEY).ok();
    let stream_options = pi_ai::types::StreamOptions {
        base: pi_ai::types::ProviderRequestOptions {
            api_key,
            ..Default::default()
        },
        ..Default::default()
    };
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
    let storage = Arc::new(Mutex::new(
        pi_agent::session::memory::InMemorySessionStorage::new(
            pi_agent::session::memory::in_memory_metadata("interactive-turn", None),
        ),
    ));
    let session = pi_agent::session::Session::<pi_agent::fs::MemoryFs>::from_in_memory(storage);
    let mut options = AgentHarnessOptions::new(session, runtime.model.clone());
    options.stream_fn = Some(stream_fn);
    options.system_prompt = runtime.system_prompt.clone();
    options.block_images = runtime.block_images;
    options.tools = Some(tools.iter().map(HarnessTool::from_agent_tool).collect());
    options.steering_mode = steering_mode;
    options.follow_up_mode = follow_up_mode;
    let (harness, _suspended) = AgentHarness::create(options)
        .await
        .map_err(|error| error.to_string())?;
    if harness
        .set_agent_messages(runtime.messages.clone())
        .await
        .is_err()
    {
        return Err("failed to seed interactive harness transcript".to_string());
    }
    let harness = Arc::new(harness);
    let agent = harness
        .agent_handle()
        .ok_or_else(|| "interactive harness has no agent".to_string())?;
    let task_harness = Arc::clone(&harness);
    let task = tokio::spawn(async move {
        let (mut new_messages, rich_events) = task_harness
            .run_prompt_with_events(vec![prompt])
            .await
            .map_err(|error| error.to_string())?;
        for event in rich_events {
            if let pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
                mut assistant_message_event,
                ..
            } = event
            {
                if let AssistantMessageEvent::Error { error_message, .. } =
                    &mut assistant_message_event
                {
                    crate::core::auth_guidance::rewrite_assistant_error(
                        error_message,
                        &provider,
                        provider_uses_oauth,
                    );
                }
                on_event(&assistant_message_event);
            }
        }
        for message in &mut new_messages {
            if let pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) = message {
                crate::core::auth_guidance::rewrite_assistant_error(
                    assistant,
                    &provider,
                    provider_uses_oauth,
                );
            }
        }
        Ok(new_messages)
    });
    Ok(InteractiveTurnWorker {
        agent,
        task,
        _idle_guard: idle_guard,
    })
}

async fn finish_interactive_turn(
    runtime: &mut InteractiveRuntime,
    new_messages: Vec<pi_agent::types::AgentMessage>,
) -> Result<Vec<pi_agent::types::AgentMessage>, String> {
    persist_messages_checked(&mut runtime.session, &new_messages).await?;
    runtime.messages.extend(new_messages.iter().cloned());
    runtime.persisted_until = runtime.messages.len();
    Ok(new_messages)
}

#[cfg_attr(not(test), allow(dead_code))]
async fn stream_turn(
    runtime: &mut InteractiveRuntime,
    message: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
) -> Result<Vec<pi_agent::types::AgentMessage>, String> {
    let worker = start_interactive_turn(runtime, message, on_event, None, None, None).await?;
    let new_messages = worker.task.await.map_err(|error| error.to_string())??;
    finish_interactive_turn(runtime, new_messages).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveQueueKind {
    Steering,
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractivePendingMessage {
    text: String,
    kind: InteractiveQueueKind,
}

struct InteractiveStreamingUi<'a> {
    tree: &'a mut Tree,
    editor: &'a Arc<Mutex<Editor>>,
    transcript_md: &'a Arc<Mutex<Markdown>>,
    footer_text: &'a Arc<Mutex<Text>>,
    easter_egg_components: &'a [SharedComponent],
    stream_buffer: &'a Arc<Mutex<String>>,
    pending_text: &'a mut String,
    hide_thinking: bool,
}

impl InteractiveStreamingUi<'_> {
    fn render(&mut self, snapshot_messages: &[pi_agent::types::AgentMessage]) {
        let stream = self.stream_buffer.lock().unwrap().clone();
        let text = it::compose_transcript_with_cache_notices(
            snapshot_messages,
            self.hide_thinking,
            &stream,
            &[],
        );
        self.transcript_md.lock().unwrap().set_text(text);
        let scene = it::build_scene(
            self.transcript_md,
            self.editor,
            self.footer_text,
            None,
            self.easter_egg_components,
            self.pending_text,
        );
        self.tree.render(Some(&scene));
    }
}

struct InteractiveTurnInput<'a> {
    input: &'a mut InteractiveInputReader,
    steering_mode: &'a str,
    follow_up_mode: &'a str,
    session_environment: Option<&'a crate::core::session_env::SessionEnvironmentGuard>,
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
    let mut redraw = tokio::time::interval(std::time::Duration::from_millis(50));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = &mut turn => {
                let result = match result {
                    Ok(Ok(messages)) => finish_interactive_turn(runtime, messages).await,
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.to_string()),
                };
                return (result, Vec::new());
            }
            _ = redraw.tick() => {
                ui.render(&snapshot_messages);
            }
            event = turn_input.input.recv() => {
                let event = match event {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => return (Err(error), queued),
                    None => return (Err("terminal input reader stopped".to_string()), queued),
                };
                match event {
                    pi_tui::terminal::TerminalEvent::Resize(_, height) => {
                        ui.tree.invalidate();
                        ui.editor
                            .lock()
                            .unwrap()
                            .set_terminal_rows(height as usize);
                    }
                    pi_tui::terminal::TerminalEvent::Key(key_str) => {
                        if key_str.is_empty() {
                            ui.render(&snapshot_messages);
                            continue;
                        }
                        if ui.tree.consume_cell_size_response(&key_str) {
                            continue;
                        }
                        let key = parse_key(&key_str);
                        if key.ctrl && key.base == "c" {
                            agent.abort();
                            drop(turn);
                            return (Err("interrupted".to_string()), Vec::new());
                        }

                        let queued_text = if key.base == "enter" && key.alt && !key.ctrl {
                            let text = ui.editor.lock().unwrap().get_text();
                            ui.editor.lock().unwrap().set_text("");
                            Some((text, InteractiveQueueKind::FollowUp))
                        } else if key.base == "enter" && !key.alt && !key.ctrl {
                            let mut guard = ui.editor.lock().unwrap();
                            guard.handle_input(&key_str);
                            guard
                                .drain_submitted()
                                .map(|text| (text, InteractiveQueueKind::Steering))
                        } else {
                            let mut editor = ui.editor.lock().unwrap();
                            if is_printable_input_batch(&key_str, &key) {
                                editor.handle_input_burst(&key_str);
                            } else {
                                editor.handle_input(&key_str);
                            }
                            None
                        };

                        if let Some((text, kind)) = queued_text {
                            if !text.trim().is_empty() {
                                ui.editor.lock().unwrap().add_to_history(&text);
                                let queued_message =
                                    pi_agent::agent::user_text_prompt(&text, pi_ai::types::now_ms());
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
    let (enabled, reserve_tokens, keep_recent_tokens) = settings_manager.get_compaction_settings();
    let settings = pi_agent::harness::compaction::CompactionSettings {
        enabled,
        reserve_tokens,
        keep_recent_tokens,
    };
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
    let result = pi_agent::harness::compaction::compact(
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
    .map_err(|e| format!("{operation}: {e}"))?;

    // Replace the in-memory context: summary message + retained tail.
    let summary_msg = pi_agent::agent::user_text_prompt(
        format!("[Compaction summary]\n{}", result.summary),
        pi_ai::types::now_ms(),
    );
    let mut replaced = vec![summary_msg];
    replaced.extend(result.retained_tail.clone());
    runtime.messages = replaced;

    // Persist a compaction entry so the session file records the summary.
    runtime
        .session
        .append_entry(
            EntryNoStats::Compaction {
                id: format!("c-{}", pi_agent::session::new_id()),
                summary: result.summary.clone(),
                retained_tail: result.retained_tail,
                tokens_before: result.tokens_before,
                details: None,
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
    if let Ok(home) = std::env::var("HOME") {
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
    let mut messages = Vec::new();
    for entry in &entries {
        if let pi_agent::session::types::Entry::Message { message, .. } = entry {
            messages.push(message.clone());
        }
    }
    let cache_entries = entries
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    transcript_md
        .lock()
        .unwrap()
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

/// Compact text for a message entry (upstream truncates in tree labels).
fn short_text(message: &pi_agent::types::AgentMessage) -> String {
    match message {
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => {
            let mut text = String::new();
            for block in a.content() {
                if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                    text.push_str(t);
                }
            }
            let trimmed = text.trim();
            trimmed.chars().take(40).collect()
        }
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::User(u)) => match u.content() {
            pi_ai::types::UserContentBody::String(s) => s.chars().take(40).collect(),
            pi_ai::types::UserContentBody::Blocks(blocks) => {
                let mut text = String::new();
                for block in blocks {
                    if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                        text.push_str(t);
                    }
                }
                text.chars().take(40).collect()
            }
        },
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::ToolResult(tr)) => {
            let mut text = String::new();
            for block in tr.content() {
                if let pi_ai::types::ContentBlock::Text { text: t, .. } = block {
                    text.push_str(t);
                }
            }
            format!(
                "tool({}) {}",
                tr.tool_name(),
                text.chars().take(24).collect::<String>()
            )
        }
        _ => String::new(),
    }
}

/// Truncate a string for compact tree labels.
fn short_truncate(text: &str) -> String {
    text.chars().take(40).collect()
}

/// Label for non-message entry types in the tree view.
fn entry_type_label(entry: &pi_agent::session::types::Entry) -> &'static str {
    match entry {
        pi_agent::session::types::Entry::Message { .. } => "message",
        pi_agent::session::types::Entry::ModelChange { .. } => "model_change",
        pi_agent::session::types::Entry::ThinkingLevel { .. } => "thinking_level",
        pi_agent::session::types::Entry::ActiveTools { .. } => "active_tools",
        pi_agent::session::types::Entry::Compaction { .. } => "compaction",
        pi_agent::session::types::Entry::BranchSummary { .. } => "branch_summary",
        pi_agent::session::types::Entry::Custom { .. } => "custom",
    }
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
    let viewer = std::env::var("PI_SHARE_VIEWER_URL")
        .unwrap_or_else(|_| "https://pi.dev/session/".to_string());
    Ok(format!("Share URL: {viewer}#{gist_id}\nGist: {gist_url}"))
}

/// TUI-backed auth interaction (upstream `AuthInteraction`): notifications go
/// to the status banner; prompts temporarily leave raw mode to read a line
/// from stdin, then re-enter raw mode.
struct TuiAuthInteraction {
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
}

fn auth_prompt_message(prompt: &pi_ai::auth::AuthPrompt) -> String {
    match prompt {
        pi_ai::auth::AuthPrompt::Text {
            message,
            placeholder,
        }
        | pi_ai::auth::AuthPrompt::ManualCode {
            message,
            placeholder,
        } => {
            let mut rendered = message.clone();
            if let Some(placeholder) = placeholder {
                rendered.push_str(&format!(" ({placeholder})"));
            }
            rendered
        }
        pi_ai::auth::AuthPrompt::Secret { message, .. } => message.clone(),
        pi_ai::auth::AuthPrompt::Select { message, options } => {
            let mut rendered = message.clone();
            for (index, option) in options.iter().enumerate() {
                rendered.push_str(&format!("\n  {}. {}", index + 1, option.label));
                if let Some(description) = &option.description {
                    rendered.push_str(&format!(" — {description}"));
                }
            }
            rendered
        }
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

fn wrap_auth_line(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let next_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };
        if !current.is_empty() && next_len > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        if word.len() > width && current.is_empty() {
            let mut remaining = word;
            while remaining.len() > width {
                let split_at = remaining
                    .char_indices()
                    .nth(width)
                    .map(|(index, _)| index)
                    .unwrap_or(remaining.len());
                lines.push(remaining[..split_at].to_string());
                remaining = &remaining[split_at..];
            }
            current.push_str(remaining);
        } else {
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_auth_panel(
    terminal: &Arc<Mutex<TerminalBackend>>,
    banner: &str,
    prompt: &str,
    input: &str,
) {
    let width = terminal.lock().unwrap().width().max(24);
    let inner_width = width.saturating_sub(4).max(20);
    let mut lines = Vec::new();
    if !banner.is_empty() {
        lines.extend(wrap_auth_line(banner, inner_width));
        lines.push(String::new());
    }
    lines.extend(wrap_auth_line(prompt, inner_width));
    lines.push(String::new());
    lines.extend(wrap_auth_line(&format!("❯ {input}"), inner_width));
    let border = "─".repeat(inner_width + 2);
    let mut rendered = String::from(pi_tui::terminal::CLEAR_SCREEN_HOME);
    rendered.push_str(&format!("╭{border}╮\r\n"));
    for line in lines {
        let padding = inner_width.saturating_sub(line.chars().count());
        rendered.push_str(&format!("│ {line}{} │\r\n", " ".repeat(padding)));
    }
    rendered.push_str(&format!("╰{border}╯\r\n"));
    terminal.lock().unwrap().write_raw(&rendered);
}

/// Read a short auth answer through the terminal backend while retaining raw
/// mode. `next_event` has a bounded poll timeout, so an OAuth callback can set
/// `abort` and wake this prompt without leaving a blocked stdin reader behind.
fn prompt_terminal_with_abort(
    banner: &Arc<Mutex<String>>,
    terminal: &Arc<Mutex<TerminalBackend>>,
    prompt: &pi_ai::auth::AuthPrompt,
    abort: &AtomicBool,
) -> Result<String, String> {
    let message = auth_prompt_message(prompt);
    let banner = banner.lock().unwrap().clone();
    let secret = matches!(prompt, pi_ai::auth::AuthPrompt::Secret { .. });
    let mut answer = String::new();
    render_auth_panel(terminal, &banner, &message, "");
    let result = loop {
        if abort.load(Ordering::SeqCst) {
            break Err("Login cancelled".to_string());
        }
        let event = match terminal.lock().unwrap().next_event() {
            Ok(event) => event,
            Err(error) => break Err(format!("read auth input: {error}")),
        };
        let pi_tui::terminal::TerminalEvent::Key(raw) = event else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let key = parse_key(&raw);
        if key.base == "enter" && !key.ctrl && !key.alt {
            break Ok(answer);
        }
        if key.base == "esc" || key.base == "escape" || (key.ctrl && key.base == "c") {
            abort.store(true, Ordering::SeqCst);
            break Err("Login cancelled".to_string());
        }
        if key.base == "backspace" || (key.ctrl && key.base == "h") {
            answer.pop();
        } else if !key.ctrl && !key.alt {
            answer.push_str(&key.base);
        }
        let visible = if secret {
            "•".repeat(answer.chars().count())
        } else {
            answer.clone()
        };
        render_auth_panel(terminal, &banner, &message, &visible);
    };
    result
}

impl pi_ai::auth::AuthInteraction for TuiAuthInteraction {
    fn supports_async_prompt(&self) -> bool {
        true
    }

    fn prompt(&self, prompt: &pi_ai::auth::AuthPrompt) -> Result<String, String> {
        let abort = AtomicBool::new(false);
        let answer = prompt_terminal_with_abort(&self.banner, &self.terminal, prompt, &abort)?;
        let answer = answer.trim();
        if let pi_ai::auth::AuthPrompt::Select { options, .. } = prompt {
            // Pi's selectors accept Enter as the highlighted first option. The
            // auth prompt is rendered inline in the active TUI, so preserve
            // that same default instead of treating an empty Enter as an
            // unknown method/provider.
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
        let banner = self.banner.clone();
        let prompt = prompt.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                prompt_terminal_with_abort(&banner, &terminal, &prompt, &abort)
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
                format!("{prefix} {verification_uri} and enter code: {user_code}")
            }
            pi_ai::auth::AuthEvent::AuthUrl { url, .. } => {
                let browser = open_auth_browser(url);
                let prefix = if browser {
                    "A browser window should open."
                } else {
                    "Open this URL to sign in:"
                };
                format!("{prefix} {url}")
            }
            pi_ai::auth::AuthEvent::Progress { message } => message.clone(),
            pi_ai::auth::AuthEvent::Info { message, .. } => message.clone(),
        };
        *self.banner.lock().unwrap() = msg;
    }
}

/// Run the upstream `/login <provider>` OAuth flow: find the provider in the
/// models registry, run its OAuth login, store the credential. Returns the
/// final status message or an error.
async fn run_oauth_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
) -> Result<String, String> {
    let providers: Vec<pi_ai::models::Provider> = models
        .get_providers()
        .into_iter()
        .filter(|p| p.auth.oauth.is_some())
        .collect();
    if providers.is_empty() {
        return Err(match provider_ref {
            Some(r) => format!("no OAuth login available for provider {r:?}"),
            None => "no OAuth-capable providers registered".to_string(),
        });
    }
    let interaction = TuiAuthInteraction { banner, terminal };
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
                    description: provider
                        .auth
                        .oauth
                        .as_ref()
                        .and_then(|oauth| oauth.login_label().map(str::to_string)),
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
    let credential = oauth.login(&interaction).await?;
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
    .await?;
    Ok(format!("logged in to {provider_id} via OAuth"))
}

async fn run_api_key_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
) -> Result<String, String> {
    let providers: Vec<pi_ai::models::Provider> = models
        .get_providers()
        .into_iter()
        .filter(|provider| provider.auth.api_key.is_some())
        .collect();
    if providers.is_empty() {
        return Err("no API-key providers registered".to_string());
    }
    let interaction = TuiAuthInteraction { banner, terminal };
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
                    description: provider
                        .auth
                        .api_key
                        .as_ref()
                        .map(|auth| auth.name().to_string()),
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
    let key = interaction.prompt(&pi_ai::auth::AuthPrompt::Secret {
        message: format!("Enter API key for {}:", provider.name),
        placeholder: Some("stored securely in auth.json".to_string()),
    })?;
    if key.trim().is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
    let opts = crate::core::auth_storage::AuthOperationOptions::default();
    let provider_id = provider.id.clone();
    let key = key.trim().to_string();
    auth.modify(
        &provider_id,
        move |_| {
            let key = key.clone();
            Box::pin(async move {
                Ok(Some(crate::core::auth_storage::Credential::ApiKey {
                    key: Some(key),
                    env: None,
                }))
            })
        },
        &opts,
    )
    .await?;
    Ok(format!("logged in to {provider_id} via API key"))
}

async fn run_login(
    models: &pi_ai::models::Models,
    provider_ref: Option<&str>,
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
) -> Result<String, String> {
    let providers = models.get_providers();
    let interaction = TuiAuthInteraction {
        banner: banner.clone(),
        terminal: terminal.clone(),
    };
    let method = if let Some(provider_ref) = provider_ref {
        let provider = providers
            .iter()
            .find(|provider| {
                provider.id.eq_ignore_ascii_case(provider_ref.trim())
                    || provider.name.eq_ignore_ascii_case(provider_ref.trim())
            })
            .ok_or_else(|| format!("no OAuth login available for provider {provider_ref:?}"))?;
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
        "oauth" => run_oauth_login(models, provider_ref, banner, terminal).await,
        "api_key" => run_api_key_login(models, provider_ref, banner, terminal).await,
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
) -> Result<String, String> {
    let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
    let opts = crate::core::auth_storage::AuthOperationOptions::default();
    let credentials = auth.list(&opts).await?;
    if credentials.is_empty() && provider_ref.is_none() {
        return Ok("No stored credentials to remove. Environment variables and models.json config are unchanged.".to_string());
    }

    if let Some(provider) = provider_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        auth.delete(provider, &opts).await?;
        return Ok(format!("logged out {provider}"));
    }

    let interaction = TuiAuthInteraction { banner, terminal };
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
    auth.delete(&provider_id, &opts).await?;
    Ok(format!("logged out {provider_id}"))
}

/// Wrap a modal in a renderable SharedComponent for the frame.
fn modal_shared(modal: &mut Modal) -> SharedComponent {
    match modal {
        Modal::Model(sel) | Modal::Thinking(sel) | Modal::Theme(sel) | Modal::Fork(sel) => {
            sel.clone() as SharedComponent
        }
        Modal::ScopedModels(sel) => sel.clone() as SharedComponent,
        Modal::Settings(panel) => panel.clone() as SharedComponent,
        Modal::Resume(sel, _) => sel.clone() as SharedComponent,
    }
}

/// The interactive main loop. Returns Ok(()) on clean exit.
pub async fn run_interactive_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut settings = settings;
    let cwd = config::cwd();
    let models = {
        let base = crate::core::model_registry::builtin_models();
        match crate::core::model_config::models_json_path() {
            Some(path) => crate::core::model_registry::ModelRegistry::new(
                base,
                crate::core::model_config::ModelConfig::load(Some(&path)),
            )
            .into_models(),
            None => base,
        }
    };
    let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
    );

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
        let selected_path = config::expand_tilde_path(selector);
        if std::path::Path::new(&selected_path).is_file() {
            crate::core::session_migration::migrate_legacy_session_file(std::path::Path::new(
                &selected_path,
            ))
            .map_err(|e| format!("migrate selected session: {e}"))?;
        }
        let source = crate::run::resolve_session_metadata(&repo, selector).await?;
        if args.fork.is_some() {
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
    } else if args.continue_session || args.resume {
        let mut sessions = repo
            .list(Some(&cwd))
            .await
            .map_err(|e| format!("list sessions: {e}"))?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_at));
        let source = sessions.into_iter().next().ok_or_else(|| {
            if args.resume {
                "no sessions found to resume in this directory".to_string()
            } else {
                "no previous session found to continue in this directory".to_string()
            }
        })?;
        let session = repo
            .open(&source)
            .await
            .map_err(|e| format!("open session {}: {e}", source.id))?;
        initial_status_banner = if args.resume {
            format!(
                "resumed session {}",
                source.id.get(..8).unwrap_or(&source.id)
            )
        } else {
            format!(
                "continued session {}",
                source.id.get(..8).unwrap_or(&source.id)
            )
        };
        session
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
        session
            .set_name(Some(name))
            .await
            .map_err(|e| format!("set session name: {e}"))?;
    }
    let session_id = session.get_metadata().await.id;
    let session_name = session.get_name().await;
    let initial_thinking_level = settings
        .get_default_thinking_level()
        .map(str::to_string)
        .unwrap_or_else(|| "off".to_string());
    let agent_dir = config::get_agent_dir().to_string_lossy().into_owned();
    let extensions = load_for_mode(
        args,
        &settings,
        &cwd,
        &agent_dir,
        "interactive",
        true,
        session_name.clone(),
        initial_thinking_level.clone(),
    );
    for error in &extensions.errors {
        tracing::warn!(path = %error.path, error = %error.error, "failed to load extension");
    }

    register_loaded_native_providers(&models, &extensions)
        .map_err(|error| format!("failed to register interactive native providers: {error}"))?;

    let faux_core = if provider == "faux" {
        Some(crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        ))
    } else {
        None
    };
    let model = if provider == "faux" {
        let core = faux_core.as_ref().expect("faux core registered");
        match model_hint.as_deref() {
            Some(hint) => core
                .get_model(Some(hint.rsplit('/').next().unwrap_or(hint)))
                .cloned()
                .ok_or_else(|| format!("unknown faux model {hint:?}"))?,
            None => core
                .models
                .first()
                .cloned()
                .ok_or_else(|| "no faux model".to_string())?,
        }
    } else {
        crate::core::model_runtime::resolve_run_model_for_provider(
            &models,
            &provider,
            model_hint.as_deref(),
        )?
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
    register_interactive_themes(args, &settings, &extension_resources, &cwd);
    let system_prompt =
        interactive_system_prompt(args, &cwd, &agent_dir, &settings, &extension_resources);
    let prompt_templates = crate::run::load_prompt_templates_for_run(
        args,
        &cwd,
        std::path::Path::new(&agent_dir),
        &extension_resources,
    );
    let mut runtime = InteractiveRuntime {
        cwd: cwd.clone(),
        models,
        faux_core,
        provider: provider.clone(),
        model: model.clone(),
        scoped_models: Vec::new(),
        messages: Vec::new(),
        session,
        repo,
        session_root: session_root.clone(),
        session_id: session_id.clone(),
        session_name,
        session_persistence,
        system_prompt,
        tools_enabled: !args.no_tools,
        builtin_tools_enabled: !args.no_tools && !args.no_builtin_tools,
        native_provider_ids: loaded_native_provider_ids(&extensions),
        extensions,
        extension_resources,
        prompt_templates,
        extension_args: args.clone(),
        extension_agent_dir: agent_dir.clone(),
        auto_resize_images: settings.get_image_auto_resize(),
        block_images: settings.get_block_images(),
        persisted_until: 0,
        active_tool_names: None,
        cache_entries: Vec::new(),
    };

    // Match the upstream non-blocking startup check: the TUI becomes usable
    // immediately and the notification is added when the request completes.
    let mut version_check = if std::env::var_os("PI_OFFLINE").is_some()
        || std::env::var_os("PI_SKIP_VERSION_CHECK").is_some()
    {
        None
    } else {
        Some(tokio::spawn(
            crate::core::version_check::check_for_new_pi_version(config::VERSION),
        ))
    };

    // Match upstream startup changelog behavior: resumed sessions do not get
    // release notes, a first install records the current version silently,
    // and a version change displays only the newly released entries.
    let mut startup_changelog = None;
    let mut should_report_install_telemetry = false;
    if initial_status_banner.is_empty() {
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
        if telemetry_enabled && std::env::var_os("PI_OFFLINE").is_none() {
            tokio::spawn(crate::core::telemetry::report_install_telemetry(
                config::VERSION,
                telemetry_enabled,
            ));
        }
    }

    // Terminal + components.
    let terminal = Arc::new(Mutex::new(TerminalBackend::new()));
    let tui_mode = args
        .tui_mode
        .as_deref()
        .unwrap_or_else(|| settings.get_tui_mode());
    let use_alt_screen = tui_mode == "fullscreen";
    terminal
        .lock()
        .unwrap()
        .enter_raw_with_alt_screen(use_alt_screen)
        .map_err(|e| format!("enter raw: {e}"))?;
    let _terminal_guard = InteractiveTerminalGuard {
        terminal: terminal.clone(),
    };

    load_interactive_theme(
        settings
            .get_theme_setting()
            .unwrap_or(crate::theme::DEFAULT_THEME),
    );
    let mut hide_thinking = settings.get_hide_thinking_block();
    let mut thinking_level = initial_thinking_level;

    let mut editor = it::create_editor(cwd.clone());
    editor.set_terminal_rows(terminal.lock().unwrap().height());
    let editor: Arc<Mutex<Editor>> = Arc::new(Mutex::new(editor));

    let transcript_md: Arc<Mutex<Markdown>> = Arc::new(Mutex::new(Markdown::new(
        String::new(),
        1,
        0,
        it::tui_theme::markdown_theme(),
        None,
        None,
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

    let mut tree = Tree::new(terminal);

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

    let mut modal: Option<Modal> = None;
    let mut status_banner = initial_status_banner;
    if let Some(changelog) = startup_changelog {
        status_banner = changelog;
    }
    let stream_buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let mut streaming = false;
    let mut pending_text = String::new();
    let mut easter_egg_components: Vec<SharedComponent> = Vec::new();
    let mut easter_egg_animation_until: Option<std::time::Instant> = None;
    let mut last_ctrl_c: Option<std::time::Instant> = None;
    // Branch discovery spawns `git`; doing that once per keypress makes fast
    // pasted/typed paths lag badly enough that the queued Enter arrives after
    // the test/user-visible interaction deadline. Refresh at most once per
    // second while keeping the footer current during normal work.
    let mut footer_branch = footer::git_branch(&cwd);
    let mut footer_branch_checked_at = std::time::Instant::now();

    tree.focus(editor.clone());
    tree.query_cell_size();
    let mut input = InteractiveInputReader::start(tree.terminal_handle());

    let result = tokio::time::timeout(std::time::Duration::from_secs(24 * 60 * 60), async {
        loop {
            if easter_egg_animation_until
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
            {
                easter_egg_animation_until = None;
            }
            if theme_changed.swap(false, Ordering::Acquire) {
                transcript_md
                    .lock()
                    .unwrap()
                    .set_theme(it::tui_theme::markdown_theme());
                tree.invalidate();
            }
            if version_check.as_ref().is_some_and(|task| task.is_finished()) {
                if let Some(task) = version_check.take() {
                    if let Ok(Some(release)) = task.await {
                        status_banner = format!(
                            "Update available: pi {} — run `pi update` (https://pi.dev/changelog)",
                            release.version
                        );
                    }
                }
            }
            // 1) Compose transcript (messages + streams + status banner).
            {
                let mut md = transcript_md.lock().unwrap();
                let stream = stream_buffer.lock().unwrap().clone();
                let cache_notices = if settings.get_show_cache_miss_notices() {
                    cache_notice_timestamps(&runtime.cache_entries)
                } else {
                    Vec::new()
                };
                let composed = it::compose_transcript_with_cache_notices(
                    &runtime.messages,
                    hide_thinking,
                    &stream,
                    &cache_notices,
                );
                let mut text = composed;
                if !status_banner.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&status_banner);
                }
                md.set_text(text);
            }

            // 2) Footer.
            {
                let now = std::time::Instant::now();
                if now.duration_since(footer_branch_checked_at)
                    >= std::time::Duration::from_secs(1)
                {
                    footer_branch = footer::git_branch(&cwd);
                    footer_branch_checked_at = now;
                }
                let (usage, cache_hit_rate) = footer_usage_from_entries(&runtime.cache_entries);
                let fd = FooterData {
                    cwd: cwd.clone(),
                    branch: footer_branch.clone(),
                    session_name: runtime.session_name.clone(),
                    model_label: Some(format!("{}/{}", runtime.provider, runtime.model.name)),
                    thinking: Some(thinking_level.clone()),
                    provider_count: runtime.models.get_providers().len(),
                    usage,
                    cache_hit_rate,
                };
                let terminal_width = tree.terminal_handle().lock().unwrap().width();
                let lines = footer::render_footer(&fd, terminal_width);
                footer_text.lock().unwrap().set_text(lines.join("\n"));
            }

            // 3) Scene.
            let modal_comp: Option<SharedComponent> = match modal.as_mut() {
                Some(m) => Some(modal_shared(m)),
                None => None,
            };
            let scene = it::build_scene(
                &transcript_md,
                &editor,
                &footer_text,
                modal_comp,
                &easter_egg_components,
                &pending_text,
            );
            tree.render(Some(&scene));

            // Upstream's startup benchmark initializes and renders the real
            // TUI, gives terminal capability probes a short window to settle,
            // then restores the terminal without waiting for user input.
            if crate::config::env_flag("PI_STARTUP_BENCHMARK") {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                return Ok(());
            }

            // 4) Input.
            let ev = tokio::select! {
                event = input.recv() => {
                    event.ok_or_else(|| "terminal input reader stopped".to_string())??
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)),
                    if easter_egg_animation_until.is_some() => {
                    continue;
                }
            };
            let key_str = match ev {
                pi_tui::terminal::TerminalEvent::Key(k) => k,
                pi_tui::terminal::TerminalEvent::Resize(_w, h) => {
                    tree.invalidate();
                    editor.lock().unwrap().set_terminal_rows(h as usize);
                    continue;
                }
            };
            if key_str.is_empty() {
                continue;
            }
            if tree.consume_cell_size_response(&key_str) {
                continue;
            }
            let key = parse_key(&key_str);

            if key.ctrl && key.base == "c" {
                if streaming {
                    status_banner = "Press Ctrl+C again to quit".to_string();
                    continue;
                }
                let now = std::time::Instant::now();
                let draft = editor.lock().unwrap().get_text();
                if !draft.is_empty() {
                    if last_ctrl_c.is_some_and(|previous| {
                        now.duration_since(previous) <= std::time::Duration::from_millis(500)
                    }) {
                        return Ok(());
                    }
                    editor.lock().unwrap().set_text("");
                    status_banner = "Input cleared. Press Ctrl+C again to quit".to_string();
                    last_ctrl_c = Some(now);
                    continue;
                }
                return Ok(());
            }
            last_ctrl_c = None;
            let editor_text = editor.lock().unwrap().get_text();
            if should_exit_on_key(&key, &editor_text) {
                return Ok(());
            }

            if modal.is_none() && !streaming && key.ctrl && key.base == "p" {
                let cycled_model = cycle_scoped_model(&mut runtime, &mut settings);
                if cycled_model.is_some() {
                    maybe_add_daxnuts_component(
                        &runtime,
                        &mut easter_egg_components,
                        &mut easter_egg_animation_until,
                    );
                }
                status_banner = cycled_model.unwrap_or_else(|| {
                    if runtime.scoped_models.is_empty() {
                        "No scoped models configured; use /scoped-models first".to_string()
                    } else {
                        "Only one scoped model configured".to_string()
                    }
                });
                _session_environment.set_model(&runtime.provider, &runtime.model.name);
                continue;
            }

            // Modal input handling.
            if let Some(active_modal) = &mut modal {
                let mut close_modal = false;
                match active_modal {
                    Modal::Model(sel) => {
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    if let Some((p, id)) = it::apply_model_selection(&mut settings, &item.value) {
                                        runtime.provider = p.clone();
                                        if let Some(m) = runtime.models.get_model(&p, &id) {
                                            runtime.model = m;
                                        }
                                        _session_environment
                                            .set_model(&runtime.provider, &runtime.model.name);
                                        status_banner = format!("Model: {}", item.label);
                                        maybe_add_daxnuts_component(
                                            &runtime,
                                            &mut easter_egg_components,
                                            &mut easter_egg_animation_until,
                                        );
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
                    Modal::ScopedModels(sel) => {
                        let mut guard = sel.lock().unwrap();
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
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    settings.set_default_thinking_level(&item.value);
                                    thinking_level = item.value.clone();
                                    hide_thinking = item.value == "off";
                                    _session_environment.set_reasoning_level(&thinking_level);
                                    status_banner = format!("Thinking: {}", item.value);
                                }
                                close_modal = true;
                            }
                            it::selectors::SelectorAction::Cancel | it::selectors::SelectorAction::Select(_) => {
                                close_modal = true;
                            }
                            _ => {}
                        }
                    }
                    Modal::Theme(sel) => {
                        let mut guard = sel.lock().unwrap();
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
                            let mut guard = sel.lock().unwrap();
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
                            if let Some(text) = result.editor_text {
                                editor.lock().unwrap().set_text(&text);
                            }
                            status_banner = result.status;
                        }
                    }
                    Modal::Resume(sel, sessions) => {
                        let (close_resume, selected_session_id) = {
                            let mut guard = sel.lock().unwrap();
                            match guard.handle(&key) {
                                it::selectors::SelectorAction::Select(Some(idx))
                                    if idx < guard.count() =>
                                {
                                    (true, guard.selected_item().map(|item| item.value))
                                }
                                it::selectors::SelectorAction::Cancel
                                | it::selectors::SelectorAction::Select(_) => (true, None),
                                _ => (false, None),
                            }
                        };
                        if let Some(session_id) = selected_session_id {
                            if let Some(meta) = sessions.iter().find(|s| s.id == session_id) {
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
                                            shutdown_extensions_before_session_replace(
                                                &runtime,
                                                "resume",
                                                Some(&target_session_file),
                                            );
                                            runtime.session = session;
                                            runtime.session_id = meta.id.clone();
                                            runtime.session_name = None;
                                            let (messages, cache_entries) =
                                                rehydrate_transcript(&runtime, &transcript_md, hide_thinking).await;
                                            runtime.messages = messages;
                                            runtime.cache_entries = cache_entries;
                                            runtime.persisted_until = runtime.messages.len();
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
                                        }
                                    }
                                }
                            }
                        }
                        if close_resume {
                            close_modal = true;
                        }
                    }
                    Modal::Settings(panel) => {
                        let was_enter = key.base == "enter" && !key.ctrl && !key.alt;
                        {
                            let mut guard = panel.lock().unwrap();
                            let _ = was_enter;
                            guard.handle_input(&key);
                        }
                        let changes = { panel.lock().unwrap().drain_changes() };
                        for (id, value) in changes {
                            let mut theme_error = None;
                            match id.as_str() {
                                "theme" => {
                                    settings.set_theme(value.clone());
                                    if let Err(error) = load_interactive_theme_checked(&value) {
                                        theme_error = Some(error);
                                    }
                                }
                                "thinking" => {
                                    settings.set_default_thinking_level(&value);
                                    thinking_level = value.clone();
                                    _session_environment.set_reasoning_level(&thinking_level);
                                }
                                "images" => {
                                    settings.set_show_images(value == "on");
                                }
                                "cache-miss-notices" => {
                                    settings.set_show_cache_miss_notices(value == "true");
                                }
                                "install-telemetry" => {
                                    settings.set_enable_install_telemetry(value == "true");
                                }
                                _ => {}
                            }
                            status_banner = theme_error
                                .map(|error| format!("Theme failed: {error}"))
                                .unwrap_or_else(|| format!("/settings {id} → {value}"));
                        }
                        if key.base == "esc" || key.base == "escape" {
                            close_modal = true;
                        }
                    }
                }
                if close_modal {
                    modal = None;
                    tree.focus(editor.clone());
                }
                continue;
            }

            // Editor input (skip Enter/Ctrl+C which the parent handles).
            {
                let mut e = editor.lock().unwrap();
                if key.ctrl && key.base == "c" {
                    continue;
                }
                if is_printable_input_batch(&key_str, &key) {
                    e.handle_input_burst(&key_str);
                } else {
                    e.handle_input(&key_str);
                }
            }

            // Submit?
            let submitted = editor.lock().unwrap().drain_submitted();
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
                    if !lifecycle_notes.is_empty() {
                        status_banner.push_str("; ");
                        status_banner.push_str(&lifecycle_notes.join("; "));
                    }
                    continue;
                }
                let action = it::parse_submit(&submitted);
                match action {
                    SubmitAction::Prompt(prompt) => {
                        let mut pending_turns = VecDeque::from([InteractivePendingMessage {
                            text: crate::core::prompt_templates::expand_prompt_template(
                                &prompt,
                                &runtime.prompt_templates,
                            ),
                            kind: InteractiveQueueKind::Steering,
                        }]);
                        while let Some(next_turn) = pending_turns.pop_front() {
                            editor.lock().unwrap().add_to_history(&next_turn.text);
                            let message_start = runtime.messages.len();
                            streaming = true;
                            pending_text = if next_turn.kind == InteractiveQueueKind::FollowUp {
                                " … follow-up".to_string()
                            } else {
                                " …".to_string()
                            };
                            *stream_buffer.lock().unwrap() = String::new();
                            let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = {
                                let stream_buffer = stream_buffer.clone();
                                Arc::new(move |event: &AssistantMessageEvent| {
                                    if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                                        stream_buffer.lock().unwrap().push_str(delta);
                                    }
                                })
                            };
                            let (turn_result, newly_queued) = {
                                let mut ui = InteractiveStreamingUi {
                                    tree: &mut tree,
                                    editor: &editor,
                                    transcript_md: &transcript_md,
                                    footer_text: &footer_text,
                                    easter_egg_components: &easter_egg_components,
                                    stream_buffer: &stream_buffer,
                                    pending_text: &mut pending_text,
                                    hide_thinking,
                                };
                                stream_turn_with_input(
                                    &mut runtime,
                                    next_turn.text,
                                    on_event,
                                    &mut ui,
                                    InteractiveTurnInput {
                                        input: &mut input,
                                        steering_mode: settings.get_steering_mode(),
                                        follow_up_mode: settings.get_follow_up_mode(),
                                        session_environment: Some(&_session_environment),
                                    },
                                )
                                .await
                            };
                            match &turn_result {
                                Err(error) => status_banner = error.clone(),
                                Ok(messages) => {
                                    if let Some(error) = messages.iter().find_map(|message| {
                                        let pi_agent::types::AgentMessage::Core(
                                            pi_ai::types::Message::Assistant(assistant),
                                        ) = message
                                        else {
                                            return None;
                                        };
                                        assistant.error_message().map(str::to_string)
                                    }) {
                                        status_banner = error;
                                    }
                                }
                            }
                            let new_messages = runtime.messages[message_start..].to_vec();
                            append_cache_entries_from_messages(&mut runtime.cache_entries, &new_messages);
                            streaming = false;
                            pending_text = String::new();
                            *stream_buffer.lock().unwrap() = String::new();
                            pending_turns.extend(newly_queued);
                            let lifecycle_notes = apply_pending_extension_lifecycle_actions(
                                &mut runtime,
                                &mut settings,
                                &thinking_level,
                                &transcript_md,
                                hide_thinking,
                            )
                            .await;
                            if !lifecycle_notes.is_empty() {
                                status_banner = lifecycle_notes.join("; ");
                            }
                            // Auto-compaction: summarize history when the context
                            // approaches the model window (upstream compaction loop).
                            match maybe_auto_compact(&mut runtime, &settings).await {
                                Ok(true) => status_banner = "context compacted (auto)".to_string(),
                                Ok(false) => {}
                                Err(e) => status_banner = e,
                            }
                        }
                    }
                    SubmitAction::Command(command, arg) => {
                        match command.kind {
                            SlashKind::Model => {
                                if let Some(value) = arg.as_deref() {
                                    match apply_model_reference(&mut runtime, &mut settings, value) {
                                        Ok(message) => {
                                            _session_environment
                                                .set_model(&runtime.provider, &runtime.model.name);
                                            status_banner = message;
                                            maybe_add_daxnuts_component(
                                                &runtime,
                                                &mut easter_egg_components,
                                                &mut easter_egg_animation_until,
                                            );
                                        }
                                        Err(error) => status_banner = error,
                                    }
                                } else {
                                    let items =
                                        it::selectors::model_selector_items(&runtime.models, None);
                                    modal = Some(Modal::Model(Arc::new(Mutex::new(
                                        ListSelector::new_slash_layout(items, 10),
                                    ))));
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
                                let items = it::selectors::thinking_selector_items();
                                modal = Some(Modal::Thinking(Arc::new(Mutex::new(ListSelector::new(items, 6)))));
                            }
                            SlashKind::Theme => {
                                let items = it::selectors::theme_selector_items();
                                modal = Some(Modal::Theme(Arc::new(Mutex::new(ListSelector::new(items, 10)))));
                            }
                            SlashKind::Settings => {
                                let entries = it::selectors::settings_selector_items(&settings);
                                modal = Some(Modal::Settings(Arc::new(Mutex::new(SettingsPanel::new(entries)))));
                            }
                            SlashKind::Session => {
                                status_banner = session_status(&runtime);
                            }
                            SlashKind::Changelog => {
                                status_banner = changelog_status();
                            }
                            SlashKind::Clear => {
                                runtime.messages.clear();
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
                                transcript_md.lock().unwrap().set_text("");
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
                                let term = tree.terminal_handle();
                                input.stop_worker().await;
                                let auth_result = if input.pending_cancel() {
                                    Err("Login cancelled".to_string())
                                } else {
                                    run_login(&runtime.models, provider_ref, banner, term).await
                                };
                                match auth_result {
                                    Ok(message) => status_banner = message,
                                    Err(error) => status_banner = error,
                                }
                                input.restart().await;
                                tree.terminal_handle()
                                    .lock()
                                    .unwrap()
                                    .write_raw(pi_tui::terminal::CLEAR_SCREEN_HOME);
                                tree.invalidate();
                            }
                            SlashKind::Logout => {
                                let provider_ref = arg
                                    .as_deref()
                                    .map(|s| s.trim())
                                    .filter(|s| !s.is_empty());
                                let banner = Arc::new(Mutex::new(String::new()));
                                let term = tree.terminal_handle();
                                input.stop_worker().await;
                                let logout_result = if input.pending_cancel() {
                                    Err("Logout cancelled".to_string())
                                } else {
                                    run_oauth_logout(&runtime.models, provider_ref, banner, term)
                                        .await
                                };
                                match logout_result {
                                    Ok(message) => status_banner = message,
                                    Err(error) => status_banner = format!("logout failed: {error}"),
                                }
                                input.restart().await;
                                tree.terminal_handle()
                                    .lock()
                                    .unwrap()
                                    .write_raw(pi_tui::terminal::CLEAR_SCREEN_HOME);
                                tree.invalidate();
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
                                    Ok(true) => status_banner = "context compacted".to_string(),
                                    Ok(false) => status_banner = "nothing to compact".to_string(),
                                    Err(error) => status_banner = error,
                                }
                            }
                            SlashKind::Debug => {
                                let terminal_handle = tree.terminal_handle();
                                let terminal = terminal_handle.lock().unwrap();
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
                                easter_egg_animation_until = Some(
                                    std::time::Instant::now()
                                        + it::easter_eggs::animation_duration(),
                                );
                            }
                            SlashKind::DementedDelves => {
                                easter_egg_components.push(it::easter_eggs::earendil_component());
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
                                            runtime.session = new_session;
                                            runtime.session_id = new_id;
                                            runtime.session_name = None;
                                            runtime.messages.clear();
                                            runtime.cache_entries.clear();
                                            runtime.persisted_until = 0;
                                            clear_easter_egg_components(
                                                &mut easter_egg_components,
                                                &mut easter_egg_animation_until,
                                            );
                                            transcript_md.lock().unwrap().set_text("");
                                            let notes = replace_extensions(
                                                &mut runtime,
                                                &settings,
                                                &thinking_level,
                                                "new",
                                                previous_target,
                                                target,
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
                                            Ok(_) => match runtime.repo.list(Some(&runtime.cwd)).await {
                                                Ok(sessions) if !sessions.is_empty() => {
                                                    // Exclude the current session so the picker offers
                                                    // other sessions (newest-first default).
                                                    let sessions = resumable_sessions(sessions, &runtime.session_id);
                                                    if sessions.is_empty() {
                                                        status_banner =
                                                            "no sessions found to resume in this directory"
                                                                .to_string();
                                                    } else {
                                                        let picker = it::session_picker_items(sessions);
                                                        let items = it::picker_select_items(&picker);
                                                        modal = Some(Modal::Resume(
                                                            Arc::new(Mutex::new(ListSelector::new(
                                                                items, 10,
                                                            ))),
                                                            picker,
                                                        ));
                                                    }
                                                }
                                                Ok(_) => {
                                                    status_banner =
                                                        "no sessions found to resume in this directory".to_string();
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
                                        Some(name) if !name.trim().is_empty() => {
                                            match runtime.session.set_name(Some(name.trim())).await {
                                                Ok(()) => {
                                                    runtime.session_name = Some(name.trim().to_string());
                                                    status_banner = format!("session name: {}", name.trim());
                                                }
                                                Err(e) => {
                                                    status_banner = format!("set name failed: {e}");
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
                                    let theme_before = settings
                                        .get_theme_setting()
                                        .unwrap_or(crate::theme::DEFAULT_THEME)
                                        .to_string();
                                    settings.reload().await;
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
                                    register_interactive_themes(
                                        &runtime.extension_args,
                                        &settings,
                                        &runtime.extension_resources,
                                        &runtime.cwd,
                                    );
                                    load_interactive_theme(
                                        settings
                                            .get_theme_setting()
                                            .unwrap_or(crate::theme::DEFAULT_THEME),
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
                                            if let Some(text) = result.editor_text {
                                                editor.lock().unwrap().set_text(&text);
                                            }
                                            status_banner = result.status;
                                        }
                                        None => {
                                            status_banner = "Nothing to clone yet".to_string();
                                        }
                                    }
                                    }
                                }
                                SlashKind::Trust => {
                                    match arg.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
                                        Some(choice) if matches!(choice.as_str(), "allow" | "deny" | "ask") => {
                                            settings.set_default_project_trust(&choice);
                                            status_banner = format!("default project trust: {choice}");
                                        }
                                        _ => {
                                            status_banner = "usage: /trust <allow|deny|ask>".to_string();
                                        }
                                    }
                                }
                                SlashKind::Copy => {
                                    // Copy the last assistant message text. Without a system
                                    // clipboard binary the text is surfaced in the banner instead.
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
                                        let copied = ["xclip", "wl-copy", "pbcopy"]
                                            .iter()
                                            .find_map(|bin| {
                                                let Ok(mut child) = std::process::Command::new(bin)
                                                    .stdin(std::process::Stdio::piped())
                                                    .spawn()
                                                else {
                                                    return None;
                                                };
                                                let mut stdin = child.stdin.take()?;
                                                use std::io::Write as _;
                                                let _ = stdin.write_all(text.as_bytes());
                                                drop(stdin);
                                                child.wait().ok();
                                                Some(())
                                            });
                                        if copied.is_some() {
                                            status_banner = "copied last assistant message to clipboard".to_string();
                                        } else {
                                            let preview: String = text.chars().take(90).collect();
                                            if preview != text {
                                                status_banner = format!("copied (preview): {preview}…");
                                            } else {
                                                status_banner = format!("copied: {preview}");
                                            }
                                        }
                                    }
                                }
                                SlashKind::Tree => {
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
                                        .await;
                                    match entries {
                                        Err(_) => {
                                            status_banner = "tree: failed to read session entries".to_string();
                                        }
                                        Ok(entries) => {
                                            // Parent-linked textual tree (same linkage as the RPC
                                            // get_tree surface, rendered compactly).
                                            let mut lines: Vec<String> = Vec::new();
                                            let mut depth: std::collections::HashMap<String, usize> =
                                                std::collections::HashMap::new();
                                            let mut by_id: std::collections::HashMap<String, String> =
                                                std::collections::HashMap::new();
                                            for entry in &entries {
                                                let id = entry.id().to_string();
                                                let parent = entry.parent_id().map(|s| s.to_string());
                                                let label = match entry {
                                                    pi_agent::session::types::Entry::Message { message, .. } => {
                                                        format!("{}: {}", message.role(), short_text(message))
                                                    }
                                                    pi_agent::session::types::Entry::Compaction { summary, .. } => {
                                                        format!("compaction: {}", short_truncate(summary))
                                                    }
                                                    pi_agent::session::types::Entry::BranchSummary { summary, .. } => {
                                                        format!("branch-summary: {}", short_truncate(summary))
                                                    }
                                                    pi_agent::session::types::Entry::ModelChange { model_id, .. } => {
                                                        format!("model_change: {model_id}")
                                                    }
                                                    other => entry_type_label(other).to_string(),
                                                };
                                                let d = parent
                                                    .as_deref()
                                                    .and_then(|p| depth.get(p))
                                                    .copied()
                                                    .unwrap_or(0);
                                                depth.insert(id.clone(), d);
                                                by_id.insert(
                                                    id.clone(),
                                                    format!("{}{} {}", "  ".repeat(d), id.get(..8).unwrap_or(&id), label),
                                                );
                                            }
                                            for entry in &entries {
                                                let id = entry.id().to_string();
                                                lines.push(by_id.get(&id).cloned().unwrap_or_default());
                                            }
                                            if lines.is_empty() {
                                                status_banner = "tree: empty session".to_string();
                                            } else {
                                                let total = lines.join("\n");
                                                let preview: String = total.chars().take(700).collect();
                                                status_banner = format!("session tree:\n{preview}");
                                            }
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

    // Leave the alternate screen.
    tree.leave_alt_screen();
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("interactive mode timed out".to_string()),
    }
}

/// The terminal reader may coalesce adjacent printable input.  Named key
/// strings are also printable ASCII, so exclude the complete key-string
/// vocabulary before treating a multi-character event as text.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{ExtensionHostAction, ExtensionHostActions};
    use pi_agent::fs::StdFileSystem;
    use pi_agent::session::jsonl::repo::CreateOptions;
    use pi_agent::session::state::ForkOptions;
    use pi_agent::session::JsonlSessionRepo;

    #[test]
    fn ctrl_d_exits_only_for_an_empty_editor() {
        let ctrl_d = parse_key("\x04");
        assert!(should_exit_on_key(&ctrl_d, ""));
        assert!(!should_exit_on_key(&ctrl_d, "draft"));
        assert!(!should_exit_on_key(&parse_key("ctrl+c"), ""));
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
        assert!(resumable_sessions(sessions, &runtime.session_id).is_empty());
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
                events_for_handler.lock().unwrap().push(event.clone());
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
            *events.lock().unwrap(),
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
                *seen_args_for_handler.lock().unwrap() =
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
        assert_eq!(*seen_args.lock().unwrap(), "first second");
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
                    .unwrap()
                    .push("before_switch".to_string());
                Ok(None)
            },
        );
        extension
            .handlers
            .insert("session_before_switch".to_string(), vec![before_handler]);
        let shutdown_handler: crate::core::extensions::HandlerFn = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, _: &Value| {
                shutdown_events.lock().unwrap().push("shutdown".to_string());
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
            *lifecycle_events.lock().unwrap(),
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
            assignments, 6,
            "all interactive replacement paths are covered"
        );
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
        let _lock = crate::theme::test_theme_registry_lock().lock().unwrap();
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
        let models =
            pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let faux_core = Some(crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        ));
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
            native_provider_ids: Vec::new(),
            extensions,
            extension_resources,
            prompt_templates: Vec::new(),
            extension_args: Args {
                no_extensions: true,
                ..Default::default()
            },
            extension_agent_dir: cwd.clone(),
            auto_resize_images: true,
            block_images: false,
            persisted_until: 0,
            active_tool_names: None,
            cache_entries: Vec::new(),
        }
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
                deltas_for_event.lock().unwrap().push(delta.clone());
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
        assert!(!deltas.lock().unwrap().is_empty());
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
}
