//! Interactive TUI mode — port of `packages/coding-agent/src/modes/interactive/
//! interactive-mode.ts` (the core loop over the ported pi-tui subset).
//!
//! Renders the message transcript + an input bar, edits the prompt inline,
//! ships it through the agent loop, and streams the assistant response into
//! the transcript. Session persistence uses the same JSONL repo as the run
//! path.

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

use pi_tui::components::{BoxComponent, Input, ScrollView, Text};
use pi_tui::keys::match_key;
use pi_tui::terminal::TerminalBackend;
use pi_tui::{Component, Scene, Tree};

/// Interactive session runtime (reuses the run/RPC wiring).
struct InteractiveRuntime {
    cwd: String,
    models: pi_ai::models::Models,
    provider: String,
    model: Model,
    messages: Vec<pi_agent::types::AgentMessage>,
    session: JsonlSession<pi_agent::fs::StdFileSystem>,
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

fn render_message(message: &pi_agent::types::AgentMessage) -> String {
    match message {
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::User(u)) => {
            let text = pi_agent::agent::user_content_text(u);
            format!("You: {text}")
        }
        pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => {
            let parts: Vec<String> = a
                .content()
                .iter()
                .filter_map(|b| match b {
                    pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            format!("π: {}", parts.join(""))
        }
        _ => String::new(),
    }
}

/// The interactive main loop. Returns Ok(()) on clean exit.
pub async fn run_interactive_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
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
            id: Some(session_id),
            cwd: cwd.clone(),
            parent_session_id: None,
            metadata: None,
            fork_options: ForkOptions::Tree,
        })
        .await
        .map_err(|e| format!("create session: {e}"))?;

    let mut runtime = InteractiveRuntime {
        cwd,
        models,
        provider,
        model,
        messages: Vec::new(),
        session,
        system_prompt: args.system_prompt.clone(),
        tools_enabled: !args.no_tools,
    };

    // Terminal + components.
    let mut terminal = TerminalBackend::new();
    terminal.enter_raw().map_err(|e| format!("enter raw: {e}"))?;

    let message_text: Arc<Mutex<Text>> = Arc::new(Mutex::new(Text::new(String::new(), 1, 1, None)));
    let _scroll_view: pi_tui::SharedComponent = Arc::new(Mutex::new(ScrollView::new(
        message_text.clone() as pi_tui::SharedComponent,
    )));
    let input: Arc<Mutex<Input>> = Arc::new(Mutex::new(Input::new("> ")));

    let mut tree = Tree::new(Arc::new(Mutex::new(terminal)));

    let render_scene = |message_text: &Arc<Mutex<Text>>,
                        input: &Arc<Mutex<Input>>,
                        pending: &str| -> Arc<Mutex<Scene>> {
        let mut children: Vec<pi_tui::SharedComponent> = Vec::new();
        children.push(message_text.clone() as pi_tui::SharedComponent);
        children.push(Arc::new(Mutex::new(pi_tui::components::Spacer::new(1))));
        if !pending.is_empty() {
            children.push(Arc::new(Mutex::new(pi_tui::components::Loader::new(pending))));
        }
        children.push(Arc::new(Mutex::new(BoxComponent::new(
            input.clone() as pi_tui::SharedComponent,
            None,
        ))));
        Arc::new(Mutex::new(Scene::new(children, None)))
    };

    let mut pending_text = String::new();
    let mut scene = render_scene(&message_text, &input, "");
    tree.focus(input.clone());

    let result = tokio::time::timeout(std::time::Duration::from_secs(24 * 60 * 60), async {
        // Runner: handles prompt submission with a stream task.
        let mut streaming = false;
        loop {
            // Render.
            {
                let text_guard = message_text.lock().unwrap();
                let mut rendered = String::new();
                let mut count = 0usize;
                for m in runtime.messages.iter() {
                    let line = render_message(m);
                    if !line.is_empty() {
                        rendered.push_str(&line);
                        rendered.push('\n');
                        count += 1;
                        if count >= 400 {
                            rendered.push_str("… (truncated)");
                            break;
                        }
                    }
                }
                drop(text_guard);
                message_text.lock().unwrap().set_text(rendered.clone());
            }
            let snapshot = render_scene(&message_text, &input, &pending_text);
            scene = snapshot.clone();
            tree.render(Some(&scene));

            // Read input (refresh at least every 100ms to repaint the loader).
            let mut got_input = false;
            for _ in 0..2 {
                let term = tree.terminal_handle();
                let ev = term.lock().unwrap().next_event().map_err(|e| e.to_string())?;
                let key_str = match ev {
                    pi_tui::terminal::TerminalEvent::Key(k) => k,
                    pi_tui::terminal::TerminalEvent::Resize(_, _) => String::new(),
                };
                if key_str.is_empty() {
                    continue;
                }
                got_input = true;
                let key = pi_tui::keys::parse_key(&key_str);
                if match_key(&key, "ctrl+c") {
                    return Ok(());
                }
                if match_key(&key, "esc") {
                    // Esc is reserved for potential overlay/alt transitions.
                    continue;
                }
                if match_key(&key, "enter") {
                    let submitted = {
                        let mut input_guard = input.lock().unwrap();
                        let value = input_guard.value.clone();
                        input_guard.set_value("");
                        value
                    };
                    if !submitted.trim().is_empty() && !streaming {
                        streaming = true;
                        let _ = streaming;
                        pending_text = " …".to_string();
                        let msg = submitted.trim().to_string();
                        let on_event: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = {
                            let message_text = message_text.clone();
                            Arc::new(move |event: &AssistantMessageEvent| {
                                if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                                    let mut guard = message_text.lock().unwrap();
                                    // Live-append deltas to the rendered transcript
                                    // (final messages render authoritative text).
                                    let mut prev = guard.text().to_string();
                                    prev.push_str(delta);
                                    guard.set_text(prev);
                                }
                            })
                        };
                        let new_messages = stream_turn(&mut runtime, msg, on_event).await;
                        streaming = false;
                        pending_text = String::new();
                        let _ = new_messages;
                    }
                    continue;
                }
                {
                    let mut input_guard = input.lock().unwrap();
                    input_guard.handle_input(&key);
                }
                break;
            }
            if !got_input {
                // Repaint for the animated loader.
                let _ = &mut tree;
            }
        }
    })
    .await;

    // Persist the last turn if any messages were produced.
    if !runtime.messages.is_empty() {
        let mut to_append: Vec<pi_agent::types::AgentMessage> = Vec::new();
        for m in runtime.messages.iter().cloned() {
            to_append.push(m);
        }
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
