//! Non-interactive run path — the `pi -p` / `pi <message>` flow. Wires the
//! provider (faux for tests; other providers as they are ported), the agent
//! loop, and session persistence.

use std::sync::Arc;

use pi_agent::agent::{run_agent_loop, AgentContext, AgentLoopConfig};
use pi_agent::session::types::EntryNoStats;
use pi_agent::session::{CreateOptions, JsonlSessionRepo};

use crate::args::Args;
use crate::config;

pub struct RunOutcome {
    pub final_text: String,
    pub session_path: Option<String>,
}

pub async fn run(args: &Args) -> Result<RunOutcome, String> {
    let provider = config::resolve_provider(args.provider.as_deref());
    // Model resolution: explicit provider/model from CLI/env; only `faux`
    // has a Rust provider today. Mirror upstream defaults (provider google)
    // but fail clearly for unported providers.
    if provider != "faux" && provider != "google" {
        return Err(format!("provider {provider:?} is not yet ported (only faux is available)"));
    }
    if provider == "google" {
        return Err(
            "the google provider is not yet ported; use --provider faux or PI_PROVIDER=faux for this build".to_string(),
        );
    }
    let model_hint = config::resolve_model(args.model.as_deref());
    let core = pi_ai::providers::FauxProviderCore::new(&pi_ai::providers::RegisterFauxProviderOptions::default());
    let model = match model_hint.as_deref() {
        Some(hint) => {
            // Accept provider/model patterns; fall back to the default model.
            let id = hint.rsplit('/').next().unwrap_or(hint);
            core.get_model(Some(id))
                .cloned()
                .ok_or_else(|| format!("unknown faux model {id:?}"))?
        }
        None => core.models.first().cloned().ok_or_else(|| "no faux model".to_string())?,
    };

    // Queue a scripted response echoing the last prompt (the faux provider
    // needs queued responses; the P4 smoke path returns a fixed answer).
    let reply = args
        .messages
        .last()
        .cloned()
        .unwrap_or_else(|| "Hello from pi-rust".to_string());
    core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
        pi_ai::providers::faux_assistant_message(
            vec![pi_ai::types::ContentBlock::text(format!(
                "faux response to: {reply}"
            ))],
            pi_ai::providers::FauxAssistantOptions::default(),
        ),
    )]);

    let cwd = config::cwd();
    let system_prompt = args.system_prompt.clone().unwrap_or_else(|| "".to_string());

    let mut context = AgentContext {
        system_prompt: Some(system_prompt),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let prompts: Vec<pi_agent::types::AgentMessage> = args
        .messages
        .iter()
        .map(|m| pi_agent::agent::user_text_prompt(m.clone(), pi_ai::types::now_ms()))
        .collect();

    let stream_fn: Arc<dyn Fn(&pi_ai::model::Model, &pi_ai::types::Context) -> pi_ai::AssistantMessageEventStream + Send + Sync> =
        Arc::new({
            let core = core.clone();
            move |model, ctx| core.stream(model, ctx, None)
        });
    let cfg = AgentLoopConfig {
        model,
        stream_fn,
        signal: None,
        stop_after_turn: true,
    };

    let mut events: Vec<pi_agent::agent::AgentEvent> = Vec::new();
    let new_messages = run_agent_loop(prompts, &mut context, &cfg, &mut |event| events.push(event)).await;

    // Extract final assistant text.
    let final_text = new_messages
        .iter()
        .rev()
        .find_map(|m| match m {
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => {
                let text: Vec<String> = a
                    .content()
                    .iter()
                    .filter_map(|b| match b {
                        pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                if text.is_empty() { None } else { Some(text.join("")) }
            }
            _ => None,
        })
        .unwrap_or_default();

    // Persist the session unless --no-session.
    let mut session_path = None;
    if !args.no_session {
        let session_root = args
            .session_dir
            .clone()
            .map(|d| config::expand_tilde_path(&d))
            .unwrap_or_else(|| config::get_session_dir().to_string_lossy().into_owned());
        std::fs::create_dir_all(&session_root).map_err(|e| format!("create session dir: {e}"))?;
        let mut repo = JsonlSessionRepo::new(pi_agent::fs::StdFileSystem::new(&cwd), &session_root);
        let id = args
            .session_id
            .clone()
            .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
            .unwrap_or_else(pi_agent::session::new_id);
        let mut session = repo
            .create(CreateOptions {
                id: Some(id.clone()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: pi_agent::session::ForkOptions::Tree,
            })
            .await
            .map_err(|e| format!("create session: {e}"))?;
        for message in &new_messages {
            session
                .storage_mut()
                .append_entry(
                    EntryNoStats::Message {
                        id: format!("m-{}", pi_agent::session::new_id()),
                        message: message.clone(),
                        terminate: None,
                    },
                    "main",
                )
                .await
                .map_err(|e| format!("append entry: {e}"))?;
        }
        if let Some(name) = &args.name {
            let _ = session.set_name(Some(name)).await;
        }
        let meta = session.get_metadata().await;
        session_path = Some(meta.path);
    }

    Ok(RunOutcome { final_text, session_path })
}
