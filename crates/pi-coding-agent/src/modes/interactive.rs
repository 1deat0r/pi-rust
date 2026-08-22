//! Interactive TUI mode — port of `packages/coding-agent/src/modes/interactive/
//! interactive-mode.ts` using the ported pi-tui component surface.
//!
//! Drives the Editor (multi-line, history, undo, autocomplete), the Markdown
//! transcript, slash-command dispatch with model/thinking/theme/settings
//! selectors, a footer, and the agent turn loop.

use std::sync::{Arc, Mutex};

use pi_agent::agent::{run_agent_loop, AgentContext, AgentLoopConfig};
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::session::Session as JsonlSession;
use pi_agent::session::state::ForkOptions;
use pi_agent::session::types::EntryNoStats;
use pi_agent::session::JsonlSessionRepo;
use pi_ai::model::Model;
use pi_ai::types::AssistantMessageEvent;

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;
use crate::interactive as it;
use crate::interactive::footer::{self, FooterData};
use crate::interactive::selectors::ListSelector;
use crate::interactive::settings_panel::SettingsPanel;
use crate::interactive::slash::SlashKind;
use crate::interactive::{Modal, SubmitAction};

use pi_tui::components::{Editor, Markdown, Text};
use pi_tui::keys::parse_key;

use pi_tui::terminal::TerminalBackend;
use pi_tui::tui::{Component, SharedComponent, Tree};

/// Interactive session runtime (reuses the run/RPC wiring).
struct InteractiveRuntime {
    cwd: String,
    models: pi_ai::models::Models,
    provider: String,
    model: Model,
    messages: Vec<pi_agent::types::AgentMessage>,
    session: JsonlSession<pi_agent::fs::StdFileSystem>,
    repo: JsonlSessionRepo<pi_agent::fs::StdFileSystem>,
    session_id: String,
    session_name: Option<String>,
    system_prompt: Option<String>,
    tools_enabled: bool,
}

/// Stream a prompt through the agent loop, observing raw events.
async fn stream_turn(
    runtime: &mut InteractiveRuntime,
    message: String,
    on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>,
) -> Vec<pi_agent::types::AgentMessage> {
    let prompt = pi_agent::agent::user_text_prompt(message.clone(), pi_ai::types::now_ms());
    runtime.messages.push(prompt.clone());
    let mut context = AgentContext::new(runtime.system_prompt.clone(), Vec::new());
    if runtime.tools_enabled {
        context.tools.push(pi_agent::tools::bash_tool(runtime.cwd.clone()));
        context.tools.push(pi_agent::tools::read_tool(runtime.cwd.clone()));
        context.tools.push(pi_agent::tools::write_tool(runtime.cwd.clone()));
        context.tools.push(pi_agent::tools::edit_tool(runtime.cwd.clone()));
        context.tools.push(crate::core::tools::ls_tool(runtime.cwd.clone()));
        context.tools.push(crate::core::tools::find_tool(runtime.cwd.clone()));
        context.tools.push(crate::core::tools::grep_tool(runtime.cwd.clone()));
    }
    let models = runtime.models.clone();
    let api_key = std::env::var(config::ENV_KEY).ok();
    let stream_options = pi_ai::types::StreamOptions {
        base: pi_ai::types::ProviderRequestOptions { api_key, ..Default::default() },
        ..Default::default()
    };
    let provider = runtime.provider.clone();
    let stream_fn: crate::run::StreamFn = if provider == "faux" {
        let core = pi_ai::providers::FauxProviderCore::new(&pi_ai::providers::RegisterFauxProviderOptions::default());
        core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
            pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(format!("faux response to: {message}"))],
                pi_ai::providers::FauxAssistantOptions::default(),
            ),
        )]);
        Arc::new(move |model, ctx| core.stream(model, ctx, None))
    } else {
        Arc::new(move |model, ctx| models.stream(model, ctx, Some(&stream_options)))
    };
    let cfg = AgentLoopConfig {
        model: runtime.model.clone(),
        stream_fn,
        signal: None,
        stop_after_turn: true,
        on_stream_event: Some(on_event),
    };
    let new_messages = run_agent_loop(vec![prompt], &mut context, &cfg, &mut |_| {}).await;
    for m in new_messages.iter().skip(1) {
        runtime.messages.push(m.clone());
    }
    new_messages
}

/// Short cwd for banners (home-relative like the footer).
fn meta_short_cwd(cwd: &str) -> String {
    if let Some(home) = std::env::var("HOME").ok() {
        if let Some(rest) = cwd.strip_prefix(&home) {
            if rest.is_empty() {
                return "~".to_string();
            }
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}

/// Wrap a modal in a renderable SharedComponent for the frame.
fn modal_shared(modal: &mut Modal) -> SharedComponent {
    match modal {
        Modal::Model(sel) | Modal::Thinking(sel) | Modal::Theme(sel) => sel.clone() as SharedComponent,
        Modal::Settings(panel) => panel.clone() as SharedComponent,
        Modal::Resume(sel, _) => sel.clone() as SharedComponent,
    }
}

/// The interactive main loop. Returns Ok(()) on clean exit.
pub async fn run_interactive_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut settings = settings;
    let cwd = config::cwd();
    let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
    let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
    );
    let model = if provider == "faux" {
        crate::run::build_faux_model(model_hint.as_deref())?
    } else {
        crate::core::model_runtime::resolve_run_model_for_provider(&models, &provider, model_hint.as_deref())?
    };

    // Session repo + initial session.
    let session_root = args
        .session_dir
        .clone()
        .map(|d| config::expand_tilde_path(&d))
        .unwrap_or_else(|| config::get_session_dir().to_string_lossy().into_owned());
    std::fs::create_dir_all(&session_root).map_err(|e| format!("create session dir: {e}"))?;
    let mut repo = JsonlSessionRepo::new(pi_agent::fs::StdFileSystem::new(&cwd), &session_root);
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
        .map_err(|e| format!("create session: {e}"))?;

    let mut runtime = InteractiveRuntime {
        cwd: cwd.clone(),
        models,
        provider: provider.clone(),
        model: model.clone(),
        messages: Vec::new(),
        session,
        repo,
        session_id: session_id.clone(),
        session_name: None,
        system_prompt: args.system_prompt.clone(),
        tools_enabled: !args.no_tools,
    };

    // Terminal + components.
    let mut terminal = TerminalBackend::new();
    terminal.enter_raw().map_err(|e| format!("enter raw: {e}"))?;

    it::tui_theme::load_theme(settings.get_theme_setting().unwrap_or(crate::theme::DEFAULT_THEME));
    let mut hide_thinking = settings.get_hide_thinking_block();
    let mut thinking_level: String = settings
        .get_default_thinking_level()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "off".to_string());

    let mut editor = it::create_editor(cwd.clone());
    editor.set_terminal_rows(terminal.height());
    let editor: Arc<Mutex<Editor>> = Arc::new(Mutex::new(editor));

    let transcript_md: Arc<Mutex<Markdown>> = Arc::new(Mutex::new(Markdown::new(
        String::new(),
        1,
        0,
        it::tui_theme::markdown_theme(),
        None,
        None,
    )));

    let mut tree = Tree::new(Arc::new(Mutex::new(terminal)));

    let footer_text: Arc<Mutex<Text>> = Arc::new(Mutex::new(Text::new(String::new(), 0, 0, None)));

    let mut modal: Option<Modal> = None;
    let mut status_banner = String::new();
    let stream_buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let mut streaming = false;
    let mut pending_text = String::new();

    tree.focus(editor.clone());

    let result = tokio::time::timeout(std::time::Duration::from_secs(24 * 60 * 60), async {
        loop {
            // 1) Compose transcript (messages + streams + status banner).
            {
                let mut md = transcript_md.lock().unwrap();
                let stream = stream_buffer.lock().unwrap().clone();
                let composed = it::compose_transcript(&runtime.messages, hide_thinking, &stream);
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
                let fd = FooterData {
                    cwd: cwd.clone(),
                    branch: footer::git_branch(&cwd),
                    session_name: runtime.session_name.clone(),
                    model_label: Some(format!("{}/{}", runtime.provider, runtime.model.name)),
                    thinking: Some(thinking_level.clone()),
                    provider_count: runtime.models.get_providers().len(),
                };
                let lines = footer::render_footer(&fd, 80);
                footer_text.lock().unwrap().set_text(lines.join("\n"));
            }

            // 3) Scene.
            let modal_comp: Option<SharedComponent> = match modal.as_mut() {
                Some(m) => Some(modal_shared(m)),
                None => None,
            };
            let scene = it::build_scene(&transcript_md, &editor, &footer_text, modal_comp, &pending_text);
            tree.render(Some(&scene));

            // 4) Input.
            let term = tree.terminal_handle();
            let ev = term.lock().unwrap().next_event().map_err(|e| e.to_string())?;
            let key_str = match ev {
                pi_tui::terminal::TerminalEvent::Key(k) => k,
                pi_tui::terminal::TerminalEvent::Resize(_w, h) => {
                    editor.lock().unwrap().set_terminal_rows(h as usize);
                    continue;
                }
            };
            if key_str.is_empty() {
                continue;
            }
            let key = parse_key(&key_str);

            if key.ctrl && key.base == "c" {
                if streaming {
                    status_banner = "Press Ctrl+C again to quit".to_string();
                    continue;
                }
                return Ok(());
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
                                        status_banner = format!("Model: {}", item.label);
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
                    Modal::Thinking(sel) => {
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    settings.set_default_thinking_level(&item.value);
                                    thinking_level = item.value.clone();
                                    hide_thinking = item.value == "off";
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
                                    it::tui_theme::load_theme(&item.value);
                                    status_banner = format!("Theme: {}", item.value);
                                }
                                close_modal = true;
                            }
                            it::selectors::SelectorAction::Cancel | it::selectors::SelectorAction::Select(_) => {
                                close_modal = true;
                            }
                            _ => {}
                        }
                    }
                    Modal::Resume(sel, sessions) => {
                        let mut guard = sel.lock().unwrap();
                        match guard.handle(&key) {
                            it::selectors::SelectorAction::Select(Some(idx)) if idx < guard.count() => {
                                if let Some(item) = guard.selected_item() {
                                    if let Some(meta) = sessions.iter().find(|s| s.id == item.value) {
                                        match runtime.repo.open(&meta.metadata).await {
                                            Ok(session) => {
                                                runtime.session = session;
                                                runtime.session_id = meta.id.clone();
                                                runtime.session_name = None;
                                                // Rehydrate the transcript from the session's message entries
                                                // (oldest first), matching the RPC get_entries load path.
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
                                                runtime.messages.clear();
                                                for entry in &entries {
                                                    if let pi_agent::session::types::Entry::Message { message, .. } = entry {
                                                        runtime.messages.push(message.clone());
                                                    }
                                                }
                                                transcript_md
                                                    .lock()
                                                    .unwrap()
                                                    .set_text(it::compose_transcript(&runtime.messages, hide_thinking, ""));
                                                status_banner = format!(
                                                    "resumed session {} ({} prior messages)",
                                                    meta.id.get(..8).unwrap_or(&meta.id),
                                                    runtime.messages.len()
                                                );
                                            }
                                            Err(e) => {
                                                status_banner = format!("resume failed: {e}");
                                            }
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
                    Modal::Settings(panel) => {
                        let was_enter = key.base == "enter" && !key.ctrl && !key.alt;
                        {
                            let mut guard = panel.lock().unwrap();
                            let _ = was_enter;
                            guard.handle_input(&key);
                        }
                        let changes = { panel.lock().unwrap().drain_changes() };
                        for (id, value) in changes {
                            match id.as_str() {
                                "theme" => {
                                    settings.set_theme(value.clone());
                                    it::tui_theme::load_theme(&value);
                                }
                                "thinking" => {
                                    settings.set_default_thinking_level(&value);
                                    thinking_level = value.clone();
                                }
                                "images" => {
                                    settings.set_show_images(value == "on");
                                }
                                _ => {}
                            }
                            status_banner = format!("/settings {id} → {value}");
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
                e.handle_input(&key_str);
            }

            // Submit?
            let submitted = editor.lock().unwrap().drain_submitted();
            if let Some(submitted) = submitted {
                if submitted.trim().is_empty() || streaming {
                    continue;
                }
                let action = it::parse_submit(&submitted);
                match action {
                    SubmitAction::Prompt(prompt) => {
                        editor.lock().unwrap().add_to_history(&prompt);
                        streaming = true;
                        pending_text = " …".to_string();
                        *stream_buffer.lock().unwrap() = String::new();
                        let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = {
                            let stream_buffer = stream_buffer.clone();
                            Arc::new(move |event: &AssistantMessageEvent| {
                                if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                                    stream_buffer.lock().unwrap().push_str(delta);
                                }
                            })
                        };
                        let _ = stream_turn(&mut runtime, prompt, on_event).await;
                        streaming = false;
                        pending_text = String::new();
                        *stream_buffer.lock().unwrap() = String::new();
                    }
                    SubmitAction::Command(command, _arg) => {
                        match command.kind {
                            SlashKind::Model => {
                                let items = it::selectors::model_selector_items(&runtime.models, None);
                                modal = Some(Modal::Model(Arc::new(Mutex::new(ListSelector::new_slash_layout(items, 10)))));
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
                                status_banner = format!(
                                    "session {} — {} messages in transcript",
                                    runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                                    runtime.messages.len()
                                );
                            }
                            SlashKind::Clear => {
                                runtime.messages.clear();
                                transcript_md.lock().unwrap().set_text("");
                            }
                            SlashKind::Hotkeys => {
                                status_banner = "hotkeys: enter submit · shift+enter newline · ctrl+c quit · ↑/↓ history · ctrl+w word-delete".to_string();
                            }
                            SlashKind::Help => {
                                status_banner = "commands: /settings /model /thinking /theme /session /compact /clear /hotkeys /help /quit".to_string();
                            }
                            SlashKind::Quit => {
                                return Ok(());
                            }
                            SlashKind::Compact => {
                                status_banner = "manual compaction lands with the harness loop wiring; use the RPC /compact in the meantime".to_string();
                            }
                            SlashKind::Unsupported => match command.name {
                                "export" => {
                                    let meta = runtime.session.get_metadata().await;
                                    match crate::core::export_html::export_session_file(
                                        &meta.path,
                                        _arg.as_deref(),
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
                                "new" => {
                                    let new_id = pi_agent::session::new_id();
                                    match runtime
                                        .repo
                                        .create(CreateOptions {
                                            id: Some(new_id.clone()),
                                            cwd: runtime.cwd.clone(),
                                            parent_session_id: None,
                                            metadata: None,
                                            fork_options: ForkOptions::Tree,
                                        })
                                        .await
                                    {
                                        Ok(new_session) => {
                                            runtime.session = new_session;
                                            runtime.session_id = new_id;
                                            runtime.messages.clear();
                                            transcript_md.lock().unwrap().set_text("");
                                            status_banner = format!(
                                                "started new session {} in {}",
                                                runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                                                meta_short_cwd(&runtime.cwd)
                                            );
                                        }
                                        Err(e) => {
                                            status_banner = format!("new session failed: {e}");
                                        }
                                    }
                                }
                                "resume" => {
                                    match runtime.repo.list(Some(&runtime.cwd)).await {
                                        Ok(sessions) if !sessions.is_empty() => {
                                            // Exclude the current session so the picker offers
                                            // other sessions (newest-first default).
                                            let sessions: Vec<_> = sessions
                                                .into_iter()
                                                .filter(|s| s.id != runtime.session_id)
                                                .collect();
                                            let picker = it::session_picker_items(sessions);
                                            let items = it::picker_select_items(&picker);
                                            modal = Some(Modal::Resume(
                                                Arc::new(Mutex::new(ListSelector::new(items, 10))),
                                                picker,
                                            ));
                                        }
                                        Ok(_) => {
                                            status_banner =
                                                "no sessions found to resume in this directory".to_string();
                                        }
                                        Err(e) => {
                                            status_banner = format!("list sessions failed: {e}");
                                        }
                                    }
                                }
                                "name" => {
                                    match _arg.as_deref() {
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
                                _ => {
                                    status_banner = format!(
                                        "`/{}` is not wired in the interactive port yet",
                                        command.name
                                    );
                                }
                            },
                        }
                    }
                }
            }
        }
    })
    .await;

    // Persist the last turn if any messages were produced.
    if !runtime.messages.is_empty() {
        let to_append: Vec<pi_agent::types::AgentMessage> = runtime.messages.to_vec();
        for message in to_append {
            let _ = runtime
                .session
                .append_entry(
                    EntryNoStats::Message {
                        id: format!("m-{}", pi_agent::session::new_id()),
                        message,
                        terminate: None,
                    },
                    "main",
                )
                .await;
        }
    }

    // Leave the alternate screen.
    tree.leave_alt_screen();
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("interactive mode timed out".to_string()),
    }
}
