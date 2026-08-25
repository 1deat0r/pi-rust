//! JSON-event mode — port of `packages/coding-agent/src/modes/json-event.ts`
//! + the `--mode json` dispatch in `main.ts` / `print-mode.ts`.
//!
//! Runs the prompt through the agent loop and emits every session event as a
//! JSON line on stdout (the session header first, when a session is written),
//! using the same event envelope as the RPC protocol.

use std::sync::Arc;

use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::session::context::{build_session_context, SessionContextBuildOptions};
use pi_ai::types::{ContentBlock, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

/// Ensure a JSON-mode extension runtime is invalidated on every exit after it
/// has been loaded, including harness setup and prompt errors.
struct JsonExtensionGuard(Arc<crate::core::extensions::ExtensionRunner>);

impl Drop for JsonExtensionGuard {
    fn drop(&mut self) {
        let _ = self.0.emit_session_shutdown("quit");
        self.0.invalidate(Some("json mode shutdown"));
    }
}

/// Keep the external extension host's idle condition aligned with the one
/// detached JSON-mode agent run. `waitForIdle()` is a real condition wait, so
/// it must observe the run as busy even though JSON mode has no interactive
/// event loop to own that state.
struct JsonIdleGuard(Arc<crate::core::extensions::ExtensionHostState>);

impl JsonIdleGuard {
    fn new(host: Arc<crate::core::extensions::ExtensionHostState>) -> Self {
        host.set_idle(false);
        Self(host)
    }
}

impl Drop for JsonIdleGuard {
    fn drop(&mut self) {
        self.0.set_idle(true);
    }
}

/// Run `--mode json`: stream the prompt and emit JSON event lines.
pub async fn run_json_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let cwd = config::cwd();
    let agent_dir = config::get_agent_dir();
    let (session, _) = crate::run::prepare_run_session(args, &cwd).await?;
    let loaded_extensions = crate::core::extensions::load_for_mode(
        args,
        &settings,
        &cwd,
        &agent_dir.to_string_lossy(),
        "json",
        false,
        session.get_name().await,
        args.thinking
            .clone()
            .or_else(|| settings.get_default_thinking_level().map(str::to_owned))
            .unwrap_or_else(|| "medium".to_string()),
    );
    let _extension_guard = JsonExtensionGuard(loaded_extensions.runner.clone());
    for error in &loaded_extensions.errors {
        tracing::warn!(path = %error.path, error = %error.error, "failed to load extension");
    }

    let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
    );

    let mut selected_provider_uses_oauth = false;
    let (model, stream_fn): (pi_ai::model::Model, crate::run::StreamFn) = if provider == "faux" {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let core = crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        crate::core::extensions::register_loaded_native_providers(&models, &loaded_extensions)
            .map_err(|error| format!("register extension providers: {error}"))?;
        let model = match model_hint.as_deref() {
            Some(hint) => {
                let id = hint.rsplit('/').next().unwrap_or(hint);
                core.get_model(Some(id))
                    .cloned()
                    .ok_or_else(|| format!("unknown faux model {id:?}"))?
            }
            None => core
                .models
                .first()
                .cloned()
                .ok_or_else(|| "no faux model".to_string())?,
        };
        // `runPrintMode` sends the initial prompt and every additional
        // positional message as separate sequential turns. Keep the faux
        // provider queue aligned with that contract so deterministic tests
        // exercise the same turn boundaries as real providers.
        let mut replies = Vec::new();
        let stdin_content = args.stdin_content.as_deref().unwrap_or_default();
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_reply = format!("{stdin_content}{first_message}");
        if !initial_reply.is_empty() {
            replies.push(initial_reply);
        } else if args.messages.is_empty() {
            replies.push("Hello from pi-rust".to_string());
        }
        replies.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .cloned(),
        );
        core.set_responses(
            replies
                .into_iter()
                .map(|reply| {
                    pi_ai::providers::FauxResponseStep::Message(
                        pi_ai::providers::faux_assistant_message(
                            vec![pi_ai::types::ContentBlock::text(format!(
                                "faux response to: {reply}"
                            ))],
                            pi_ai::providers::FauxAssistantOptions::default(),
                        ),
                    )
                })
                .collect(),
        );
        let stream_models = models.clone();
        let stream_fn: crate::run::StreamFn = Arc::new(move |model, ctx| {
            stream_models.stream(model, ctx, Some(&pi_ai::types::StreamOptions::default()))
        });
        (model, stream_fn)
    } else {
        let models = {
            let models = crate::core::model_registry::builtin_models();
            let config = crate::core::model_config::ModelConfig::load(
                crate::core::model_config::models_json_path().as_deref(),
            );
            let registry = crate::core::model_registry::ModelRegistry::new(models, config);
            registry.into_models()
        };
        crate::core::extensions::register_loaded_native_providers(&models, &loaded_extensions)
            .map_err(|error| format!("register extension providers: {error}"))?;
        if models.get_provider(&provider).is_none() {
            return Err(format!(
                "provider {provider:?} is not registered in the model registry"
            ));
        }
        selected_provider_uses_oauth = models
            .get_provider(&provider)
            .is_some_and(|registered| registered.auth.oauth.is_some());
        let model = crate::core::model_runtime::resolve_run_model_for_provider(
            &models,
            &provider,
            model_hint.as_deref(),
        )?;
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
        let stream_fn: crate::run::StreamFn =
            Arc::new(move |_model, ctx| models.stream(_model, ctx, Some(&stream_options)));
        (model, stream_fn)
    };

    let prepared_files = crate::run::prepare_file_arguments(
        &args.file_args,
        &cwd,
        settings.get_image_auto_resize(),
    )?;
    let mut prompt_inputs: Vec<(String, Vec<ContentBlock>)> = Vec::new();
    let stdin_content = args.stdin_content.as_deref().unwrap_or_default();
    if let Some((file_text, images)) = prepared_files {
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_text = format!("{stdin_content}{file_text}{first_message}");
        if !initial_text.is_empty() || !images.is_empty() {
            prompt_inputs.push((initial_text, images));
        }
        prompt_inputs.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .map(|text| (text.clone(), Vec::new())),
        );
    } else {
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_text = format!("{stdin_content}{first_message}");
        if !initial_text.is_empty() {
            prompt_inputs.push((initial_text, Vec::new()));
        }
        prompt_inputs.extend(
            args.messages
                .iter()
                .skip(usize::from(!args.messages.is_empty()))
                .map(|text| (text.clone(), Vec::new())),
        );
    }
    let prompts: Vec<pi_agent::types::AgentMessage> = prompt_inputs
        .into_iter()
        .map(|(text, images)| {
            let mut blocks = vec![ContentBlock::text(text)];
            blocks.extend(images);
            pi_agent::types::AgentMessage::Core(Message::User(UserContent::blocks(
                blocks,
                pi_ai::types::now_ms(),
            )))
        })
        .collect();

    let mut tools: Vec<pi_agent::tools::AgentTool> = Vec::new();
    if !args.no_tools && !args.no_builtin_tools {
        tools.extend([
            pi_agent::tools::bash_tool(cwd.clone()),
            pi_agent::tools::read_tool_with_options(
                cwd.clone(),
                pi_agent::tools::image::ProcessImageOptions {
                    auto_resize_images: settings.get_image_auto_resize(),
                    ..Default::default()
                },
            ),
            pi_agent::tools::write_tool(cwd.clone()),
            pi_agent::tools::edit_tool(cwd.clone()),
            crate::core::tools::ls_tool(cwd.clone()),
            crate::core::tools::find_tool(cwd.clone()),
            crate::core::tools::grep_tool(cwd.clone()),
        ]);
    }
    crate::core::extensions::install_tools(&loaded_extensions, &mut tools, !args.no_tools);

    let mut options = AgentHarnessOptions::new(session, model);
    options.stream_fn = Some(stream_fn);
    options.system_prompt = args.system_prompt.clone();
    options.block_images = settings.get_block_images();
    options.tools = Some(tools.iter().map(HarnessTool::from_agent_tool).collect());
    let (harness, _) = AgentHarness::create(options)
        .await
        .map_err(|error| error.to_string())?;
    let existing_entries = harness
        .transcript()
        .await
        .map_err(|error| error.to_string())?;
    if !existing_entries.is_empty() {
        let context =
            build_session_context(&existing_entries, &SessionContextBuildOptions::default());
        harness
            .set_agent_messages(context.messages)
            .await
            .map_err(|error| error.to_string())?;
    }
    let _idle_guard = JsonIdleGuard::new(loaded_extensions.host.clone());
    // Match the upstream print-mode loop: each positional message is its own
    // prompt, agent turn, and persisted assistant response. Passing the whole
    // vector to one harness call would batch the user messages into a single
    // turn and would be observably different in JSON mode.
    let mut rich_events = Vec::new();
    for prompt in prompts {
        let (_, events) = harness
            .run_prompt_with_events(vec![prompt])
            .await
            .map_err(|error| error.to_string())?;
        rich_events.extend(events);
    }

    // Emit the captured events in wire order. A streamed terminal model error
    // is delivered as a JSON event line and the process exits 0 — upstream
    // `runPrintMode` only treats Error/Aborted as a nonzero exit in *text*
    // mode, never in json mode.
    for event in rich_events {
        if let Some(line) = crate::modes::rpc::serialize_rpc_prompt_event_with_auth(
            event,
            Some(&provider),
            selected_provider_uses_oauth,
        ) {
            print!("{line}");
        }
    }

    Ok(())
}

/// Serialize a JSON value as a single line (upstream `serializeJsonLine`).
pub fn serialize_json_line(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_line_serialization() {
        assert_eq!(
            serialize_json_line(&json!({"type": "x"})),
            r#"{"type":"x"}"#
        );
    }
}
