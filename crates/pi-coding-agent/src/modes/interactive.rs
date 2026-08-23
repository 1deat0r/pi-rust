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
    /// Number of in-memory messages already persisted into the current
    /// session. Session-switch operations (resume/fork/clone) advance it so
    /// the exit persist only appends messages added after the switch.
    persisted_until: usize,
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
    // Seed the model context with prior history (the current prompt is passed
    // separately below); without this each turn would only see its own prompt.
    context.messages = runtime.messages[..runtime.messages.len() - 1].to_vec();
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

/// Rehydrate in-memory messages + transcript from a session's message
/// entries (oldest first), mirroring the RPC get_entries load path.
async fn rehydrate_transcript(
    runtime: &InteractiveRuntime,
    transcript_md: &Arc<Mutex<Markdown>>,
    hide_thinking: bool,
) -> Vec<pi_agent::types::AgentMessage> {
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
    transcript_md
        .lock()
        .unwrap()
        .set_text(it::compose_transcript(&messages, hide_thinking, ""));
    messages
}

/// Append in-memory messages to a session's main lane (idempotent per call).
async fn persist_messages(
    session: &mut JsonlSession<pi_agent::fs::StdFileSystem>,
    messages: &[pi_agent::types::AgentMessage],
) {
    for message in messages {
        let _ = session
            .append_entry(
                EntryNoStats::Message {
                    id: format!("m-{}", pi_agent::session::new_id()),
                    message: message.clone(),
                    terminate: None,
                },
                "main",
            )
            .await;
    }
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
            format!("tool({}) {}", tr.tool_name(), text.chars().take(24).collect::<String>())
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
    let gh_auth = match run_gh(vec!["auth".to_string(), "status".to_string()]).await {
        Ok(out) => out,
        Err(_) => {
            return Err("GitHub CLI (gh) is not installed. Install it from https://cli.github.com/".to_string())
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
    let viewer = std::env::var("PI_SHARE_VIEWER_URL").unwrap_or_else(|_| "https://pi.dev/session/".to_string());
    Ok(format!("Share URL: {viewer}#{gist_id}\nGist: {gist_url}"))
}

/// TUI-backed auth interaction (upstream `AuthInteraction`): notifications go
/// to the status banner; prompts temporarily leave raw mode to read a line
/// from stdin, then re-enter raw mode.
struct TuiAuthInteraction {
    banner: Arc<Mutex<String>>,
    terminal: Arc<Mutex<TerminalBackend>>,
}

impl pi_ai::auth::AuthInteraction for TuiAuthInteraction {
    fn prompt(&self, prompt: &pi_ai::auth::AuthPrompt) -> Result<String, String> {
        let message = match prompt {
            pi_ai::auth::AuthPrompt::Text { message, placeholder } => {
                let mut m = message.clone();
                if let Some(p) = placeholder {
                    m.push_str(&format!(" ({p})"));
                }
                m
            }
            pi_ai::auth::AuthPrompt::Secret { message, .. } => message.clone(),
            pi_ai::auth::AuthPrompt::ManualCode { message, placeholder } => {
                let mut m = message.clone();
                if let Some(p) = placeholder {
                    m.push_str(&format!(" ({p})"));
                }
                m
            }
            pi_ai::auth::AuthPrompt::Select { message, options } => {
                let mut m = message.clone();
                for (i, opt) in options.iter().enumerate() {
                    m.push_str(&format!("\n  {}. {}", i + 1, opt.label));
                }
                m
            }
        };
        let mut terminal = self.terminal.lock().unwrap();
        terminal.leave_raw().map_err(|e| format!("leave raw: {e}"))?;
        println!("\n{message}");
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("read input: {e}"))?;
        terminal.enter_raw().map_err(|e| format!("enter raw: {e}"))?;
        Ok(line.trim().to_string())
    }

    fn notify(&self, event: &pi_ai::auth::AuthEvent) {
        let msg = match event {
            pi_ai::auth::AuthEvent::DeviceCode { user_code, verification_uri, .. } => {
                format!("Open {verification_uri} and enter code: {user_code}")
            }
            pi_ai::auth::AuthEvent::AuthUrl { url, .. } => format!("Open this URL to sign in: {url}"),
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
        .filter(|p| match provider_ref {
            Some(r) => p.id == r || p.name.as_str() == r,
            None => true,
        })
        .collect();
    if providers.is_empty() {
        return Err(match provider_ref {
            Some(r) => format!("no OAuth login available for provider {r:?}"),
            None => "no OAuth-capable providers registered".to_string(),
        });
    }
    let provider = &providers[0];
    let oauth = provider.auth.oauth.as_ref().expect("filtered for oauth");
    let interaction = TuiAuthInteraction { banner, terminal };
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
        persisted_until: 0,
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
                                                runtime.messages = rehydrate_transcript(&runtime, &transcript_md, hide_thinking).await;
                                                runtime.persisted_until = runtime.messages.len();
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
                                            runtime.persisted_until = 0;
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
                                "import" => {
                                    let mut import_path: Option<String> = None;
                                    match _arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                        None => {
                                            status_banner = "usage: /import <session.jsonl>".to_string();
                                        }
                                        Some(path) => {
                                            if !std::path::Path::new(path).exists() {
                                                status_banner = format!("file not found: {path}");
                                            } else if let Ok(content) = std::fs::read_to_string(path) {
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
                                                        import_path = Some(path.to_string());
                                                        let metadata = pi_agent::session::types::SessionMetadata {
                                                            id: header_id,
                                                            created_at: 0,
                                                            cwd: runtime.cwd.clone(),
                                                            path: path.to_string(),
                                                            modified_at: 0,
                                                            source_format: 4,
                                                            parent_session_id: None,
                                                            legacy_parent_session_path: None,
                                                            metadata: None,
                                                        };
                                                        match runtime.repo.open(&metadata).await {
                                                            Ok(session) => {
                                                                runtime.session = session;
                                                                runtime.session_id =
                                                                    runtime.session.get_metadata().await.id;
                                                                runtime.session_name = None;
                                                                runtime.messages = rehydrate_transcript(
                                                                    &runtime,
                                                                    &transcript_md,
                                                                    hide_thinking,
                                                                )
                                                                .await;
                                                                runtime.persisted_until = runtime.messages.len();
                                                                status_banner = format!(
                                                                    "imported {} ({} prior messages)",
                                                                    path,
                                                                    runtime.messages.len()
                                                                );
                                                            }
                                                            Err(e) => {
                                                                status_banner = format!("import failed: {e}");
                                                            }
                                                        }
                                                    }
                                                }
                                            } else {
                                                status_banner = format!("cannot read {path}");
                                            }
                                        }
                                    }
                                    let _ = &import_path;
                                }
                                "reload" => {
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
                                    it::tui_theme::load_theme(
                                        settings
                                            .get_theme_setting()
                                            .unwrap_or(crate::theme::DEFAULT_THEME),
                                    );
                                    let theme_after = settings
                                        .get_theme_setting()
                                        .unwrap_or(crate::theme::DEFAULT_THEME)
                                        .to_string();
                                    if theme_after != theme_before {
                                        notes.push(format!("theme changed to {theme_after}"));
                                    }
                                    if notes.is_empty() {
                                        status_banner = "reloaded settings".to_string();
                                    } else {
                                        status_banner = format!("reloaded settings ({})", notes.join("; "));
                                    }
                                }
                                "fork" | "clone" => {
                                    // Persist the current in-memory transcript first so the
                                    // fork/clone carries it (the interactive loop only persists
                                    // on exit; we switch sessions before that happens).
                                    if !runtime.messages.is_empty() {
                                        let to_append: Vec<pi_agent::types::AgentMessage> = runtime.messages.to_vec();
                                        persist_messages(&mut runtime.session, &to_append).await;
                                    }
                                    let meta = runtime.session.get_metadata().await;
                                    let new_id = pi_agent::session::new_id();
                                    let cwd = runtime.cwd.clone();
                                    let result = if command.name == "fork" {
                                        runtime
                                            .repo
                                            .fork(
                                                &meta,
                                                CreateOptions {
                                                    id: Some(new_id.clone()),
                                                    cwd,
                                                    parent_session_id: None,
                                                    metadata: None,
                                                    fork_options: ForkOptions::Tree,
                                                },
                                            )
                                            .await
                                    } else {
                                        let mut fresh = runtime
                                            .repo
                                            .create(CreateOptions {
                                                id: Some(new_id.clone()),
                                                cwd,
                                                parent_session_id: None,
                                                metadata: None,
                                                fork_options: ForkOptions::Tree,
                                            })
                                            .await
                                            .map_err(|e| format!("clone create failed: {e}"))?;
                                        let to_append: Vec<pi_agent::types::AgentMessage> = runtime.messages.to_vec();
                                        persist_messages(&mut fresh, &to_append).await;
                                        Ok(fresh)
                                    };
                                    match result {
                                        Ok(session) => {
                                            runtime.session = session;
                                            runtime.session_id = new_id;
                                            runtime.session_name = None;
                                            // Messages are persisted in the target already; keep the
                                            // in-memory transcript for display and only persist
                                            // messages added after the switch.
                                            runtime.persisted_until = runtime.messages.len();
                                            transcript_md
                                                .lock()
                                                .unwrap()
                                                .set_text(it::compose_transcript(
                                                    &runtime.messages,
                                                    hide_thinking,
                                                    "",
                                                ));
                                            status_banner = format!(
                                                "{} session {} ({} prior messages)",
                                                command.name,
                                                runtime.session_id.get(..8).unwrap_or(&runtime.session_id),
                                                runtime.messages.len()
                                            );
                                        }
                                        Err(e) => {
                                            status_banner = format!("{} failed: {e}", command.name);
                                        }
                                    }
                                }
                                "trust" => {
                                    match _arg.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
                                        Some(choice) if matches!(choice.as_str(), "allow" | "deny" | "ask") => {
                                            settings.set_default_project_trust(&choice);
                                            status_banner = format!("default project trust: {choice}");
                                        }
                                        _ => {
                                            status_banner = "usage: /trust <allow|deny|ask>".to_string();
                                        }
                                    }
                                }
                                "copy" => {
                                    // Copy the last assistant message text. Without a system
                                    // clipboard binary the text is surfaced in the banner instead.
                                    let mut text = String::new();
                                    for message in runtime.messages.iter().rev() {
                                        match message {
                                            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => {
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
                                            _ => {}
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
                                "login" => {
                                    let provider_ref = _arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
                                    let banner = Arc::new(Mutex::new(String::new()));
                                    let term = tree.terminal_handle();
                                    match run_oauth_login(&runtime.models, provider_ref, banner.clone(), term).await {
                                        Ok(message) => status_banner = message,
                                        Err(e) => status_banner = e,
                                    }
                                }
                                "logout" => {
                                    match _arg.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                        Some(provider) => {
                                            let auth = crate::core::auth_storage::AuthStorage::create(config::get_auth_path());
                                            let opts = crate::core::auth_storage::AuthOperationOptions::default();
                                            match auth.delete(provider, &opts).await {
                                                Ok(()) => status_banner = format!("logged out {provider}"),
                                                Err(e) => status_banner = format!("logout failed: {e}"),
                                            }
                                        }
                                        None => {
                                            status_banner = "usage: /logout <provider>".to_string();
                                        }
                                    }
                                }
                                "tree" => {
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
                                "share" => {
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


#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::fs::StdFileSystem;
    use pi_agent::session::jsonl::repo::CreateOptions;
    use pi_agent::session::JsonlSessionRepo;
    use pi_agent::session::state::ForkOptions;

    /// Serializes tests that mutate the process-global PATH /
    /// PI_SHARE_VIEWER_URL so parallel executions cannot race on the env.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap()
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
            EnvGuard { old_path, old_viewer }
        }

        fn install_hermetic(bin_dir: &std::path::Path, viewer: &str) -> Self {
            let old_path = std::env::var("PATH").unwrap_or_default();
            let old_viewer = std::env::var("PI_SHARE_VIEWER_URL").ok();
            std::env::set_var("PATH", bin_dir.as_os_str());
            std::env::set_var("PI_SHARE_VIEWER_URL", viewer);
            EnvGuard { old_path, old_viewer }
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
        let mut repo = JsonlSessionRepo::new(StdFileSystem::new(&cwd), session_root.to_string_lossy().into_owned());
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
        let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
        let model = crate::run::build_faux_model(None).unwrap();
        InteractiveRuntime {
            cwd,
            models,
            provider: "faux".to_string(),
            model,
            messages: Vec::new(),
            session,
            repo,
            session_id,
            session_name: None,
            system_prompt: None,
            tools_enabled: true,
            persisted_until: 0,
        }
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
            std::fs::set_permissions(bin_dir.join("gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[tokio::test]
    async fn share_creates_secret_gist_and_prints_viewer_url() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock();
        let runtime = test_runtime(&root).await;
        install_fake_gh(&root.join("bin"), 0, Some("https://gist.github.com/fakeuser/abc123"));
        let _guard = EnvGuard::install(&root.join("bin"), "https://pi.dev/session/");
        let msg = run_share(&runtime, false).await.expect("share should succeed");
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
        let _env = env_lock();
        let runtime = test_runtime(&root).await;
        install_fake_gh(&root.join("bin"), 1, None);
        let _guard = EnvGuard::install(&root.join("bin"), "https://pi.dev/session/");
        let err = run_share(&runtime, false).await.unwrap_err();
        assert_eq!(err, "GitHub CLI is not logged in. Run 'gh auth login' first.");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_reports_missing_gh() {
        let root = std::env::temp_dir().join(format!("pi-share-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let _env = env_lock();
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
        let _env = env_lock();
        let runtime = test_runtime(&root).await;
        let msg = run_share(&runtime, true).await.unwrap();
        assert_eq!(msg, "PI_SHARE_DRY_RUN=1: /share skipped");
        let _ = std::fs::remove_dir_all(&root);
    }
}
