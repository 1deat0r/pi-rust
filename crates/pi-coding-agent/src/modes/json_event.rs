//! JSON-event mode — port of `packages/coding-agent/src/modes/json-event.ts`
//! + the `--mode json` dispatch in `main.ts` / `print-mode.ts`.
//!
//! Runs the prompt through the agent loop and emits every session event as a
//! JSON line on stdout (the session header first, when a session is written),
//! using the same event envelope as the RPC protocol.

use std::sync::{Arc, Mutex};

use pi_agent::agent::{run_agent_loop, AgentContext, AgentLoopConfig};
use pi_ai::types::{ContentBlock, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

/// Run `--mode json`: stream the prompt and emit JSON event lines.
pub async fn run_json_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let cwd = config::cwd();
    let agent_dir = config::get_agent_dir();
    let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
    );

    let (model, stream_fn): (pi_ai::model::Model, crate::run::StreamFn) = if provider == "faux" {
        let core = pi_ai::providers::FauxProviderCore::new(
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
        let core = core.clone();
        let stream_fn: crate::run::StreamFn =
            Arc::new(move |model, ctx| core.stream(model, ctx, None));
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

    let mut context = AgentContext::new(args.system_prompt.clone(), Vec::new());
    context.block_images = settings.get_block_images();
    if !args.no_tools {
        context.tools.push(pi_agent::tools::bash_tool(cwd.clone()));
        context.tools.push(pi_agent::tools::read_tool_with_options(
            cwd.clone(),
            pi_agent::tools::image::ProcessImageOptions {
                auto_resize_images: settings.get_image_auto_resize(),
                ..Default::default()
            },
        ));
        context.tools.push(pi_agent::tools::write_tool(cwd.clone()));
        context.tools.push(pi_agent::tools::edit_tool(cwd.clone()));
        context.tools.push(crate::core::tools::ls_tool(cwd.clone()));
        context
            .tools
            .push(crate::core::tools::find_tool(cwd.clone()));
        context
            .tools
            .push(crate::core::tools::grep_tool(cwd.clone()));
    }

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

    let cfg = AgentLoopConfig {
        model,
        stream_fn,
        signal: None,
        stop_after_turn: true,
        on_stream_event: None,
    };

    // Emit each assistant event as a JSON line (upstream toJsonEvent).
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observer: Arc<dyn Fn(&pi_ai::types::AssistantMessageEvent) + Send + Sync> = {
        let sink = events.clone();
        Arc::new(move |event: &pi_ai::types::AssistantMessageEvent| {
            let update = crate::modes::rpc::to_json_message_update(event);
            sink.lock().unwrap().push(serialize_json_line(&update));
        })
    };
    let mut cfg = cfg;
    cfg.on_stream_event = Some(observer);

    run_agent_loop(prompts, &mut context, &cfg, &mut |_| {}).await;

    // Emit the captured events in wire order. A streamed terminal model error
    // is delivered as a JSON event line and the process exits 0 — upstream
    // `runPrintMode` only treats Error/Aborted as a nonzero exit in *text*
    // mode, never in json mode.
    let captured = events.lock().unwrap().drain(..).collect::<Vec<String>>();
    for line in captured {
        println!("{line}");
    }

    let _ = agent_dir;
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
