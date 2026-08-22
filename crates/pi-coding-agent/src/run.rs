//! Non-interactive run path — the `pi -p` / `pi <message>` flow. Wires the
//! provider (faux for tests; other providers as they are ported), the agent
//! loop, and session persistence.
//!
//! Provider/model resolution order (1:1 with upstream `findInitialModel` for
//! the one-shot path, plus the port's documented env surface):
//!   CLI `--provider`/`--model` → `PI_PROVIDER`/`PI_MODEL` env →
//!   settings.json `defaultProvider`/`defaultModel` (project merged over
//!   global) → hard default `google` / provider default.

use std::sync::Arc;

use pi_agent::agent::{run_agent_loop, AgentContext, AgentLoopConfig};
use pi_agent::session::types::EntryNoStats;
use pi_agent::session::{CreateOptions, JsonlSessionRepo};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

/// Provider stream function: `(model, context) -> event stream`.
pub type StreamFn =
    Arc<dyn Fn(&pi_ai::model::Model, &pi_ai::types::Context) -> pi_ai::AssistantMessageEventStream + Send + Sync>;

pub struct RunOutcome {
    pub final_text: String,
    pub session_path: Option<String>,
}

/// Provider resolution for the run path: CLI → env → settings → `google`.
pub fn resolve_run_provider(cli_provider: Option<&str>, settings: &SettingsManager) -> String {
    cli_provider
        .map(|s| s.to_string())
        .or_else(|| config::env(config::ENV_PROVIDER))
        .or_else(|| settings.get_default_provider().map(|s| s.to_string()))
        .unwrap_or_else(|| "google".to_string())
}

/// Model-hint resolution for the run path: CLI → env → settings → None.
///
/// `apply_settings_default` gates the settings stage: upstream pairs
/// settings `defaultProvider`+`defaultModel` as a unit and resolves models
/// from the provider's own scope once an explicit provider source (CLI/env)
/// is present, so the settings default model must not leak into that scope.
pub fn resolve_run_model(
    cli_model: Option<&str>,
    settings: &SettingsManager,
    apply_settings_default: bool,
) -> Option<String> {
    cli_model
        .map(|s| s.to_string())
        .or_else(|| config::env(config::ENV_MODEL))
        .or_else(|| {
            if apply_settings_default {
                settings.get_default_model().map(|s| s.to_string())
            } else {
                None
            }
        })
}

/// True when an explicit provider source (CLI flag or PI_PROVIDER env) is in
/// play; settings defaults then apply only to the model stage at most.
pub fn has_explicit_provider(cli_provider: Option<&str>) -> bool {
    cli_provider.is_some() || config::env(config::ENV_PROVIDER).is_some()
}

pub async fn run(args: &Args) -> Result<RunOutcome, String> {
    let cwd = config::cwd();
    let agent_dir = config::get_agent_dir();
    let mut settings = SettingsManager::create(
        &cwd,
        &agent_dir.display().to_string(),
        crate::core::settings::SettingsManagerCreateOptions::default(),
    );
    // Surface settings load errors as diagnostics (never silently ignore a
    // malformed settings.json that the user expects to take effect).
    let settings_errors = settings.drain_errors();
    if let Some(first) = settings_errors.first() {
        tracing::warn!(
            scope = ?first.scope,
            path = ?first.path,
            error = %first.error,
            "settings load error; continuing with defaults"
        );
    }

    let provider = resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = resolve_run_model(
        args.model.as_deref(),
        &settings,
        !has_explicit_provider(args.provider.as_deref()),
    );

    // Build the model + stream function for the selected provider. Real
    // providers route through the pi-ai Models facade (catalog-backed model
    // resolution + auth application + api dispatch). `faux` keeps its
    // scripted path for tests.
    let (model, stream_fn): (pi_ai::model::Model, StreamFn) = if provider == "faux" {
        let core = pi_ai::providers::FauxProviderCore::new(&pi_ai::providers::RegisterFauxProviderOptions::default());
        let model = match model_hint.as_deref() {
            Some(hint) => {
                let id = hint.rsplit('/').next().unwrap_or(hint);
                core.get_model(Some(id))
                    .cloned()
                    .ok_or_else(|| format!("unknown faux model {id:?}"))?
            }
            None => core.models.first().cloned().ok_or_else(|| "no faux model".to_string())?,
        };
        let reply = args
            .messages
            .last()
            .cloned()
            .unwrap_or_else(|| "Hello from pi-rust".to_string());
        core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
            pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(format!("faux response to: {reply}"))],
                pi_ai::providers::FauxAssistantOptions::default(),
            ),
        )]);
        let core = core.clone();
        let stream_fn: StreamFn = Arc::new(move |model, ctx| core.stream(model, ctx, None));
        (model, stream_fn)
    } else {
        let models = {
            let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());
            models
        };
        if models.get_provider(&provider).is_none() {
            return Err(format!("provider {provider:?} is not registered in the model registry"));
        }
        let model = crate::core::model_runtime::resolve_run_model_for_provider(&models, &provider, model_hint.as_deref())?;
        // Stream options carry the explicit --api-key / PI_KEY (the facade
        // applies env-key auth when absent).
        let api_key = args
            .api_key
            .clone()
            .or_else(|| std::env::var(config::ENV_KEY).ok());
        let stream_options = pi_ai::types::StreamOptions {
            base: pi_ai::types::ProviderRequestOptions {
                api_key,
                ..Default::default()
            },
            ..Default::default()
        };
        let models = models.clone();
        let stream_fn: StreamFn = Arc::new(move |_model, ctx| {
            models.stream(_model, ctx, Some(&stream_options))
        });
        (model, stream_fn)
    };

    let system_prompt = args.system_prompt.clone().unwrap_or_default();

    // Register built-in tools (bash/read/write/edit + ls/find/grep) unless
    // --no-tools.
    let mut tools: Vec<pi_agent::tools::AgentTool> = Vec::new();
    if !args.no_tools {
        tools.push(pi_agent::tools::bash_tool(cwd.clone()));
        tools.push(pi_agent::tools::read_tool(cwd.clone()));
        tools.push(pi_agent::tools::write_tool(cwd.clone()));
        tools.push(pi_agent::tools::edit_tool(cwd.clone()));
        tools.push(crate::core::tools::ls_tool(cwd.clone()));
        tools.push(crate::core::tools::find_tool(cwd.clone()));
        tools.push(crate::core::tools::grep_tool(cwd.clone()));
    }
    let mut context = AgentContext {
        system_prompt: Some(system_prompt),
        messages: Vec::new(),
        tools,
    };
    let prompts: Vec<pi_agent::types::AgentMessage> = args
        .messages
        .iter()
        .map(|m| pi_agent::agent::user_text_prompt(m.clone(), pi_ai::types::now_ms()))
        .collect();

    let cfg = AgentLoopConfig {
        model,
        stream_fn,
        signal: None,
        stop_after_turn: true,
    };

    let mut events: Vec<pi_agent::agent::AgentEvent> = Vec::new();
    let new_messages = run_agent_loop(prompts, &mut context, &cfg, &mut |event| events.push(event)).await;

    // Extract final assistant text; surface a provider error message as a
    // top-level error (upstream prints the error and exits nonzero).
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
    // A terminal assistant error with no visible text must surface on stderr.
    if final_text.is_empty() {
        if let Some(err) = new_messages.iter().rev().find_map(|m| match m {
            pi_agent::types::AgentMessage::Core(pi_ai::types::Message::Assistant(a)) => {
                a.error_message().map(|s| s.to_string())
            }
            _ => None,
        }) {
            return Err(format!("model error: {err}"));
        }
    }

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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::SettingsMap;
    use serde_json::json;

    fn manager() -> SettingsManager {
        SettingsManager::in_memory(serde_json::from_value(json!({
            "defaultProvider": "faux",
            "defaultModel": "faux-1"
        })).unwrap())
    }

    fn clear_env_provider_model() {
        unsafe {
            std::env::remove_var(config::ENV_PROVIDER);
            std::env::remove_var(config::ENV_MODEL);
        }
    }

    #[test]
    fn resolve_provider_cli_beats_settings() {
        clear_env_provider_model();
        let settings = SettingsManager::in_memory(serde_json::from_value(json!({
            "defaultProvider": "faux"
        })).unwrap());
        let provider = resolve_run_provider(Some("anthropic"), &settings);
        assert_eq!(provider, "anthropic");
        let provider = resolve_run_provider(None, &settings);
        assert_eq!(provider, "faux");
        let provider = resolve_run_provider(None, &SettingsManager::in_memory(SettingsMap::new()));
        assert_eq!(provider, "google");
    }

    #[test]
    fn resolve_model_settings_applies_when_no_explicit_provider() {
        clear_env_provider_model();
        let settings = manager();
        let model = resolve_run_model(None, &settings, true);
        assert_eq!(model.as_deref(), Some("faux-1"));
        let model = resolve_run_model(Some("faux-2"), &settings, true);
        assert_eq!(model.as_deref(), Some("faux-2"));
    }

    #[test]
    fn resolve_model_settings_gated_off_with_explicit_provider() {
        clear_env_provider_model();
        let settings = manager();
        // Upstream pairs defaultProvider+defaultModel; an explicit provider
        // source means the settings default model must not leak in.
        let model = resolve_run_model(None, &settings, false);
        assert_eq!(model, None);
        let model = resolve_run_model(Some("faux-2"), &settings, false);
        assert_eq!(model.as_deref(), Some("faux-2"));
    }
}
