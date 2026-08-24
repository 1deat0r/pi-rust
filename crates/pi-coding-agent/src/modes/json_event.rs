//! JSON-event mode — port of `packages/coding-agent/src/modes/json-event.ts`
//! + the `--mode json` dispatch in `main.ts` / `print-mode.ts`.
//!
//! Runs the prompt through the agent loop and emits every session event as a
//! JSON line on stdout (the session header first, when a session is written),
//! using the same event envelope as the RPC protocol.

use std::sync::{Arc, Mutex};

use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::rich_agent::RichAgentEvent;
use pi_ai::types::{ContentBlock, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

/// Run `--mode json`: stream the prompt and emit JSON event lines.
pub async fn run_json_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let cwd = config::cwd();
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

    let tools: Vec<pi_agent::tools::AgentTool> = if !args.no_tools {
        vec![
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
        ]
    } else {
        Vec::new()
    };

    let prepared_files = crate::run::prepare_file_arguments(
        &args.file_args,
        &cwd,
        settings.get_image_auto_resize(),
    )?;
    let mut prompt_inputs: Vec<(String, Vec<ContentBlock>)> = Vec::new();
    if let Some((file_text, images)) = prepared_files {
        let first_message = args.messages.first().cloned().unwrap_or_default();
        let initial_text = format!("{file_text}{first_message}");
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
        prompt_inputs.extend(args.messages.iter().map(|text| (text.clone(), Vec::new())));
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

    let storage = Arc::new(Mutex::new(
        pi_agent::session::memory::InMemorySessionStorage::new(
            pi_agent::session::memory::in_memory_metadata("json-mode", None),
        ),
    ));
    let session = pi_agent::session::Session::<pi_agent::fs::MemoryFs>::from_in_memory(storage);
    let mut options = AgentHarnessOptions::new(session, model);
    options.stream_fn = Some(stream_fn);
    options.system_prompt = args.system_prompt.clone();
    options.block_images = settings.get_block_images();
    options.tools = Some(tools.iter().map(HarnessTool::from_agent_tool).collect());
    let (mut harness, _) = AgentHarness::create(options)
        .await
        .map_err(|error| error.to_string())?;
    let (_, rich_events) = harness
        .run_prompt_with_events(prompts)
        .await
        .map_err(|error| error.to_string())?;

    // Emit the captured events in wire order. A streamed terminal model error
    // is delivered as a JSON event line and the process exits 0 — upstream
    // `runPrintMode` only treats Error/Aborted as a nonzero exit in *text*
    // mode, never in json mode.
    for event in rich_events {
        if let RichAgentEvent::MessageUpdate {
            mut assistant_message_event,
            ..
        } = event
        {
            if let pi_ai::types::AssistantMessageEvent::Error { error_message, .. } =
                &mut assistant_message_event
            {
                crate::core::auth_guidance::rewrite_assistant_error(
                    error_message,
                    &provider,
                    selected_provider_uses_oauth,
                );
            }
            let update = crate::modes::rpc::to_json_message_update(&assistant_message_event);
            println!("{}", serialize_json_line(&update));
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
