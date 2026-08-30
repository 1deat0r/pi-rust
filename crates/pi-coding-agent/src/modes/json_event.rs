//! JSON-event mode — port of `packages/coding-agent/src/modes/json-event.ts`
//! + the `--mode json` dispatch in `main.ts` / `print-mode.ts`.
//!
//! Runs the prompt through the agent loop and emits every session event as a
//! JSON line on stdout (the v3 session header first),
//! using the same event envelope as the RPC protocol.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use pi_agent::harness::{AgentHarness, AgentHarnessOptions, HarnessTool};
use pi_agent::session::context::{build_session_context, SessionContextBuildOptions};
use pi_ai::types::{AssistantMessageEvent, ContentBlock, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

type JsonEventWriter =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

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
    let (session, _) = crate::run::prepare_run_session_v3(args, &cwd).await?;
    // The upstream JSON print protocol emits SessionManager.getHeader() before
    // binding the event stream. That header and the durable JSON session use
    // the v3 shape; native non-JSON session callers retain the pi-agent v4
    // format. An in-memory `--no-session` session has the same observable
    // header and must not be confused with a durable session file.
    let metadata = session.get_metadata().await;
    let header = json_session_header(&metadata);
    crate::core::output_guard::write_raw_stdout(&serialize_json_line(&header))
        .await
        .map_err(|error| format!("stdout write failed: {error}"))?;
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

    let mut provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
    let model_hint = crate::run::resolve_run_model(
        args.model.as_deref(),
        &settings,
        !crate::run::has_explicit_provider(args.provider.as_deref()),
        Some(&provider),
    );
    let has_explicit_model = args.model.as_deref().is_some_and(|model| !model.is_empty());
    let has_existing_session =
        args.continue_session || args.resume || args.session.is_some() || args.fork.is_some();

    let mut selected_provider_uses_oauth = false;
    let (model, stream_fn): (pi_ai::model::Model, crate::run::StreamFn) = if provider == "faux" {
        let models = pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default());
        let core = crate::core::model_runtime::register_faux_provider(
            &models,
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        crate::core::extensions::register_loaded_native_providers(&models, &loaded_extensions)
            .map_err(|error| format!("register extension providers: {error}"))?;
        let (scoped_models, scope_diagnostics) =
            crate::run::resolve_effective_model_scope(args, &settings, &core.models);
        for diagnostic in scope_diagnostics {
            eprintln!("Warning: {}", diagnostic.message);
        }
        let scoped_model = if !has_explicit_model && !has_existing_session {
            scoped_models.first().map(|scoped| scoped.model.clone())
        } else {
            None
        };
        let model = if let Some(model) = scoped_model {
            provider = model.provider.clone();
            model
        } else {
            match model_hint.as_deref() {
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
            }
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
        let (scoped_models, scope_diagnostics) =
            crate::run::resolve_effective_model_scope(args, &settings, &models.get_models(None));
        for diagnostic in scope_diagnostics {
            eprintln!("Warning: {}", diagnostic.message);
        }
        let scoped_model = if !has_explicit_model && !has_existing_session {
            scoped_models.first().map(|scoped| scoped.model.clone())
        } else {
            None
        };
        if let Some(scoped_model) = &scoped_model {
            provider = scoped_model.provider.clone();
        }
        crate::core::model_runtime::register_llama_provider_if_selected(
            &models,
            &provider,
            !args.offline && !config::env_flag(config::ENV_OFFLINE),
        )
        .await?;
        if models.get_provider(&provider).is_none() {
            return Err(format!(
                "provider {provider:?} is not registered in the model registry"
            ));
        }
        crate::run::require_authenticated_implicit_model(
            &models,
            &provider,
            model_hint.as_deref(),
        )?;
        selected_provider_uses_oauth = models
            .get_provider(&provider)
            .is_some_and(|registered| registered.auth.oauth.is_some());
        let model =
            scoped_model.unwrap_or(crate::core::model_runtime::resolve_run_model_for_provider(
                &models,
                &provider,
                model_hint.as_deref(),
            )?);
        crate::core::model_runtime::refresh_provider_oauth_if_needed(&models, &provider).await?;
        let api_key = args
            .api_key
            .clone()
            .and_then(|key| config::nonempty_env_value(Some(key)))
            .or_else(|| config::nonempty_env_value(std::env::var(config::ENV_KEY).ok()));
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
    // `install_tools` intentionally publishes and returns the complete
    // registry. The CLI flags are an active-tool policy, however, so apply
    // that policy before constructing the harness; otherwise JSON mode would
    // expose every tool even when --tools/--exclude-tools was supplied.
    let all_tools = tools.clone();
    tools = crate::run::select_active_tools(args, &settings, tools);
    let extension_tool_definitions = loaded_extensions.runner.get_all_registered_tools();
    let all_tool_values = all_tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.tool.name,
                "description": tool.tool.description,
                "parameters": tool.tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    let active_tool_names = tools
        .iter()
        .map(|tool| tool.tool.name.clone())
        .collect::<Vec<_>>();
    let mut command_runner = loaded_extensions.runner.as_ref().clone();
    let commands = command_runner
        .get_registered_commands()
        .into_iter()
        .map(|command| {
            serde_json::json!({
                "name": command.invocation_name,
                "description": command.description,
            })
        })
        .collect();
    // Keep extension-facing getTools/getActiveTools snapshots in lockstep
    // with the harness. `install_tools` has already published the complete
    // registry, but its active list predates the CLI allow/deny policy.
    loaded_extensions
        .host
        .set_catalog(active_tool_names, all_tool_values, commands);

    let mut options = AgentHarnessOptions::new(session, model);
    options.stream_fn = Some(stream_fn);
    options.system_prompt = Some(crate::run::assemble_run_system_prompt_with_active_tools(
        args,
        &cwd,
        std::path::Path::new(&agent_dir),
        &settings,
        &loaded_extensions.resources,
        &tools,
        &extension_tool_definitions,
    ));
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
    let Some(agent) = harness.agent_handle() else {
        return Err("JSON mode could not access the configured agent".to_string());
    };

    // Match upstream print mode: subscribe before prompting and write each
    // event as soon as the agent emits it. The Agent dispatcher awaits this
    // listener, which preserves event order and applies stdout backpressure
    // before the next event can be delivered. The error slot lets a broken
    // pipe escape the listener's fire-and-forget callback boundary.
    let stdout_writer: JsonEventWriter = Arc::new(|line| {
        Box::pin(async move {
            crate::core::output_guard::write_raw_stdout(&line)
                .await
                .map_err(|error| error.to_string())
        })
    });
    let output_error = subscribe_json_events(
        &agent,
        &provider,
        selected_provider_uses_oauth,
        stdout_writer.clone(),
    );

    // Match the upstream print-mode loop: each positional message is its own
    // prompt, agent turn, and persisted assistant response. Passing the whole
    // vector to one harness call would batch the user messages into a single
    // turn and would be observably different in JSON mode.
    for prompt in prompts {
        let prompt_result = harness.run_prompt_with_events(vec![prompt]).await;
        if let Some(error) = output_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            return Err(format!("stdout write failed: {error}"));
        }
        prompt_result.map_err(|error| error.to_string())?;
        stdout_writer(serialize_json_line(&serde_json::json!({
            "type": "agent_settled"
        })))
        .await
        .map_err(|error| format!("stdout write failed: {error}"))?;
    }

    // A streamed terminal model error is delivered as a JSON event line and
    // the process exits 0 — upstream `runPrintMode` only treats Error/Aborted
    // as a nonzero exit in text mode, never in json mode. A transport/output
    // failure is different and must remain an actual mode error.
    if let Some(error) = output_error
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
    {
        return Err(format!("stdout write failed: {error}"));
    }

    Ok(())
}

/// Attach the JSONL event sink before a prompt starts.
///
/// The writer is deliberately injected so the subscription's timing and
/// ordering can be regression-tested without depending on a network
/// provider. Production uses the raw stdout writer; tests use an in-memory
/// sink. Each callback is awaited by the Agent dispatcher before it accepts
/// the next event, matching the upstream session backpressure boundary.
fn subscribe_json_events(
    agent: &pi_agent::rich_agent::Agent,
    provider: &str,
    provider_uses_oauth: bool,
    writer: JsonEventWriter,
) -> Arc<Mutex<Option<String>>> {
    let output_error = Arc::new(Mutex::new(None::<String>));
    let output_error_for_listener = output_error.clone();
    let adapter = Arc::new(Mutex::new(JsonEventAdapter::default()));
    let provider = provider.to_string();
    let _unsubscribe = agent.subscribe(move |event, signal| {
        let output_error = output_error_for_listener.clone();
        let adapter = adapter.clone();
        let provider = provider.clone();
        let writer = writer.clone();
        let lines = {
            let mut adapter = adapter.lock().unwrap_or_else(|error| error.into_inner());
            adapt_json_event(&mut adapter, event, Some(&provider), provider_uses_oauth)
        };
        Box::pin(async move {
            for line in lines {
                if output_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_some()
                {
                    return;
                }
                if let Err(error) = writer(line).await {
                    let mut recorded = output_error
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if recorded.is_none() {
                        *recorded = Some(error);
                        if let Some(signal) = signal.as_ref() {
                            signal.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
        })
    });
    output_error
}

#[derive(Default)]
struct JsonEventAdapter {
    pending_assistant_start: Option<String>,
    pending_tool_call_events: Vec<String>,
}

fn is_empty_assistant_start(event: &pi_agent::rich_agent::RichAgentEvent) -> bool {
    matches!(
        event,
        pi_agent::rich_agent::RichAgentEvent::MessageStart { message }
            if matches!(
                message,
                pi_agent::AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.content().is_empty()
            )
    )
}

fn tool_call_placeholder(
    event: &pi_agent::rich_agent::RichAgentEvent,
) -> Option<serde_json::Value> {
    let (id, name) = match event {
        pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            assistant_message_event:
                AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial,
                },
            ..
        }
        | pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            assistant_message_event:
                AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    partial,
                    ..
                },
            ..
        } => match partial.content().get(*content_index)? {
            ContentBlock::ToolCall { id, name, .. } => (id, name),
            _ => return None,
        },
        pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            assistant_message_event:
                AssistantMessageEvent::ToolCallEnd {
                    tool_call: ContentBlock::ToolCall { id, name, .. },
                    ..
                },
            ..
        } => (id, name),
        _ => return None,
    };
    Some(serde_json::json!({
        "type": "toolCall",
        "id": id,
        "name": name,
        "arguments": {},
        "partialArgs": "",
    }))
}

fn is_tool_call_progress(event: &pi_agent::rich_agent::RichAgentEvent) -> bool {
    matches!(
        event,
        pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::ToolCallStart { .. }
                | AssistantMessageEvent::ToolCallDelta { .. },
            ..
        }
    )
}

fn normalize_tool_use_stop_reasons(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("stopReason") == Some(&serde_json::json!("tool_use")) {
                object.insert("stopReason".to_string(), serde_json::json!("toolUse"));
            }
            for child in object.values_mut() {
                normalize_tool_use_stop_reasons(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                normalize_tool_use_stop_reasons(child);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn normalize_json_line(line: String) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
        return line;
    };
    normalize_tool_use_stop_reasons(&mut value);
    serialize_json_line(&value)
}

fn synthesize_tool_call_start(mut line: String, placeholder: serde_json::Value) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
        return line;
    };
    if let Some(message) = value
        .get_mut("message")
        .and_then(serde_json::Value::as_object_mut)
    {
        message.insert("content".to_string(), serde_json::json!([placeholder]));
    }
    normalize_tool_use_stop_reasons(&mut value);
    line = serialize_json_line(&value);
    line
}

fn adapt_json_event(
    adapter: &mut JsonEventAdapter,
    event: pi_agent::rich_agent::RichAgentEvent,
    provider: Option<&str>,
    provider_uses_oauth: bool,
) -> Vec<String> {
    let is_assistant_start = is_empty_assistant_start(&event);
    let placeholder = tool_call_placeholder(&event);
    let tool_call_progress = is_tool_call_progress(&event);
    let line = crate::modes::rpc::serialize_rpc_prompt_event_with_auth(
        event,
        provider,
        provider_uses_oauth,
    );
    let mut lines = Vec::new();

    if is_assistant_start {
        if let Some(pending) = adapter.pending_assistant_start.take() {
            lines.push(normalize_json_line(pending));
        }
        adapter.pending_assistant_start = line;
        adapter.pending_tool_call_events.clear();
        return lines;
    }

    if let Some(pending) = adapter.pending_assistant_start.take() {
        if placeholder.is_none() && tool_call_progress {
            adapter.pending_assistant_start = Some(pending);
            if let Some(line) = line {
                adapter.pending_tool_call_events.push(line);
            }
            return lines;
        }
        lines.push(match placeholder {
            Some(placeholder) => synthesize_tool_call_start(pending, placeholder),
            None => normalize_json_line(pending),
        });
        lines.append(&mut adapter.pending_tool_call_events);
    }
    if let Some(line) = line {
        lines.push(normalize_json_line(line));
    }
    lines
}

/// Serialize a JSON value as a single line (upstream `serializeJsonLine`).
pub fn serialize_json_line(value: &serde_json::Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
    )
}

/// Convert the native session metadata to the SessionHeader wire event used
/// by the upstream JSON and RPC protocols.  Storage metadata remains native
/// v4; this conversion is deliberately limited to the output boundary.
fn json_session_header(metadata: &pi_agent::session::SessionMetadata) -> serde_json::Value {
    let timestamp = crate::core::export_html::iso8601_timestamp_from_epoch_ms(metadata.created_at);
    let mut header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": metadata.id,
        "timestamp": timestamp,
        "cwd": metadata.cwd,
    });
    if let Some(parent_session) = metadata
        .parent_session_id
        .as_deref()
        .or(metadata.legacy_parent_session_path.as_deref())
    {
        header["parentSession"] = serde_json::Value::String(parent_session.to_string());
    }
    header
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_agent::agent::StreamFn;
    use pi_agent::rich_agent::Agent;
    use pi_ai::event_stream::AssistantMessageEventStream;
    use pi_ai::types::{AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason};
    use serde_json::json;
    use tokio::sync::Notify;

    #[test]
    fn json_line_serialization() {
        assert_eq!(
            serialize_json_line(&json!({"type": "x"})),
            concat!(r#"{"type":"x"}"#, "\n")
        );
    }

    #[test]
    fn json_tool_turn_has_upstream_placeholder_and_tool_use_casing() {
        let mut start = AssistantMessage::new();
        start.set_stop_reason(pi_ai::types::StopReason::Pending);
        let start_event = pi_agent::rich_agent::RichAgentEvent::MessageStart {
            message: pi_agent::AgentMessage::Core(Message::Assistant(start.clone())),
        };
        let mut partial = start.clone();
        partial.content_mut().push(ContentBlock::tool_call(
            "call-1",
            "bash",
            json!({"path": "x"}),
        ));
        partial.set_stop_reason(pi_ai::types::StopReason::ToolUse);
        let tool_start = pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            message: pi_agent::AgentMessage::Core(Message::Assistant(partial.clone())),
            assistant_message_event: AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: partial.clone(),
            },
        };
        let end = pi_agent::rich_agent::RichAgentEvent::MessageEnd {
            message: pi_agent::AgentMessage::Core(Message::Assistant(partial.clone())),
        };
        let turn_end = pi_agent::rich_agent::RichAgentEvent::TurnEnd {
            message: pi_agent::AgentMessage::Core(Message::Assistant(partial)),
            tool_results: Vec::new(),
        };

        let mut adapter = JsonEventAdapter::default();
        assert!(adapt_json_event(&mut adapter, start_event, None, false).is_empty());
        let tool_lines = adapt_json_event(&mut adapter, tool_start, None, false);
        assert_eq!(tool_lines.len(), 2);

        let message_start: serde_json::Value =
            serde_json::from_str(&tool_lines[0]).expect("valid message_start JSON");
        assert_eq!(message_start["type"], "message_start");
        assert_eq!(message_start["message"]["content"][0]["type"], "toolCall");
        assert_eq!(message_start["message"]["content"][0]["id"], "call-1");
        assert_eq!(message_start["message"]["content"][0]["name"], "bash");
        assert_eq!(
            message_start["message"]["content"][0]["arguments"],
            json!({})
        );
        assert_eq!(message_start["message"]["content"][0]["partialArgs"], "");

        let tool_update: serde_json::Value =
            serde_json::from_str(&tool_lines[1]).expect("valid tool update JSON");
        assert_eq!(
            tool_update["assistantMessageEvent"]["type"],
            "toolcall_start"
        );
        assert_eq!(tool_update["assistantMessageEvent"]["id"], "call-1");
        assert_eq!(tool_update["assistantMessageEvent"]["toolName"], "bash");

        let end_line = adapt_json_event(&mut adapter, end, None, false);
        let end_json: serde_json::Value =
            serde_json::from_str(&end_line[0]).expect("valid message_end JSON");
        assert_eq!(end_json["message"]["stopReason"], "toolUse");

        let turn_line = adapt_json_event(&mut adapter, turn_end, None, false);
        let turn_json: serde_json::Value =
            serde_json::from_str(&turn_line[0]).expect("valid turn_end JSON");
        assert_eq!(turn_json["message"]["stopReason"], "toolUse");
    }

    #[test]
    fn json_tool_placeholder_waits_for_tool_call_end_when_start_has_no_identity() {
        let mut start = AssistantMessage::new();
        start.set_stop_reason(pi_ai::types::StopReason::Pending);
        let start_event = pi_agent::rich_agent::RichAgentEvent::MessageStart {
            message: pi_agent::AgentMessage::Core(Message::Assistant(start.clone())),
        };
        let empty_partial = start.clone();
        let tool_start = pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            message: pi_agent::AgentMessage::Core(Message::Assistant(empty_partial.clone())),
            assistant_message_event: AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: empty_partial.clone(),
            },
        };
        let tool_delta = pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            message: pi_agent::AgentMessage::Core(Message::Assistant(empty_partial.clone())),
            assistant_message_event: AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"command\":\"true\"}".to_string(),
                partial: empty_partial,
            },
        };
        let mut complete = start;
        complete.content_mut().push(ContentBlock::tool_call(
            "call-late",
            "bash",
            json!({"command": "true"}),
        ));
        let tool_end = pi_agent::rich_agent::RichAgentEvent::MessageUpdate {
            message: pi_agent::AgentMessage::Core(Message::Assistant(complete.clone())),
            assistant_message_event: AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: complete.content()[0].clone(),
                partial: complete,
            },
        };

        let mut adapter = JsonEventAdapter::default();
        assert!(adapt_json_event(&mut adapter, start_event, None, false).is_empty());
        assert!(adapt_json_event(&mut adapter, tool_start, None, false).is_empty());
        assert!(adapt_json_event(&mut adapter, tool_delta, None, false).is_empty());
        let lines = adapt_json_event(&mut adapter, tool_end, None, false);
        assert_eq!(lines.len(), 4);
        let message_start: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(message_start["message"]["content"][0]["type"], "toolCall");
        assert_eq!(message_start["message"]["content"][0]["id"], "call-late");
        let start_json: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(
            start_json["assistantMessageEvent"]["type"],
            "toolcall_start"
        );
        let delta_json: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(
            delta_json["assistantMessageEvent"]["type"],
            "toolcall_delta"
        );
        let end_json: serde_json::Value = serde_json::from_str(&lines[3]).unwrap();
        assert_eq!(end_json["assistantMessageEvent"]["type"], "toolcall_end");
    }

    #[test]
    fn json_session_header_is_v3_and_precedes_agent_events_for_any_session_kind() {
        let metadata = pi_agent::session::SessionMetadata {
            id: "session-fixture".to_string(),
            created_at: 1_700_000_000_123,
            cwd: "/tmp/pi-fixture".to_string(),
            path: String::new(),
            modified_at: 0,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };
        let header = json_session_header(&metadata);
        let first = serde_json::from_str::<serde_json::Value>(&serialize_json_line(&header))
            .expect("session header is valid JSON");
        let agent_start = json!({"type": "agent_start"});
        let wire = [first, agent_start];

        assert_eq!(wire[0]["type"], "session");
        assert_eq!(wire[0]["version"], 3);
        assert_eq!(wire[0]["id"], "session-fixture");
        assert_eq!(wire[0]["timestamp"], "2023-11-14T22:13:20.123Z");
        assert_eq!(wire[0]["cwd"], "/tmp/pi-fixture");
        assert_eq!(wire[1]["type"], "agent_start");
        assert!(wire[0]["kind"].is_null());
        assert!(wire[0]["createdAt"].is_null());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn json_events_are_written_incrementally_in_agent_order() {
        let release = Arc::new(Notify::new());
        let stream_fn: StreamFn = {
            let release = release.clone();
            Arc::new(move |model, _context| {
                let stream = AssistantMessageEventStream::new();
                let sender = stream.sender().expect("new stream sender");
                let release = release.clone();
                let model = model.clone();
                tokio::spawn(async move {
                    let mut partial = AssistantMessage::new();
                    partial.set_api_provider_model(&model.api, &model.provider, &model.id);
                    partial.set_stop_reason(pi_ai::types::StopReason::Pending);
                    let _ = sender.send(AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    });
                    let mut text_partial = partial.clone();
                    text_partial
                        .content_mut()
                        .push(ContentBlock::text(String::new()));
                    let _ = sender.send(AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: text_partial.clone(),
                    });
                    if let ContentBlock::Text { text, .. } = &mut text_partial.content_mut()[0] {
                        text.push_str("partial");
                    }
                    let _ = sender.send(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "partial".to_string(),
                        partial: text_partial.clone(),
                    });

                    // Keep the provider turn open after its first update. A
                    // turn-buffered JSON mode cannot produce the update while
                    // this await is pending.
                    release.notified().await;

                    let _ = sender.send(AssistantMessageEvent::TextEnd {
                        content_index: 0,
                        content: "partial".to_string(),
                        partial: text_partial.clone(),
                    });
                    text_partial.set_stop_reason(pi_ai::types::StopReason::Stop);
                    let _ = sender.send(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message: text_partial,
                    });
                });
                stream
            })
        };
        let agent = Arc::new(Agent::new(stream_fn));
        agent.state().model = pi_ai::model::Model::new(
            "incremental-test",
            "Incremental test",
            "test-api",
            "test-provider",
        );

        let (written_tx, mut written_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let writer: JsonEventWriter = Arc::new(move |line| {
            let written_tx = written_tx.clone();
            Box::pin(async move {
                written_tx
                    .send(line)
                    .map_err(|_| "test JSON event sink closed".to_string())
            })
        });
        let writer_for_settle = writer.clone();
        let output_error = subscribe_json_events(&agent, "test-provider", false, writer);

        let running = {
            let agent = agent.clone();
            tokio::spawn(async move {
                agent
                    .prompt(pi_agent::agent::user_text_prompt("hello", 1))
                    .await
            })
        };
        tokio::pin!(running);

        let mut received = Vec::new();
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(1), written_rx.recv())
                .await
                .expect("incremental JSON event should arrive before provider EOF")
                .expect("test JSON event sink should remain open");
            let event: serde_json::Value =
                serde_json::from_str(&line).expect("JSON event line should be valid JSON");
            let is_update = event["type"] == "message_update";
            received.push(event);
            if is_update {
                break;
            }
        }

        assert!(
            !running.is_finished(),
            "the first update must arrive before the provider turn reaches EOF"
        );
        assert!(output_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none());

        release.notify_one();
        running
            .await
            .expect("prompt task should not panic")
            .expect("incremental prompt should succeed");
        writer_for_settle(serialize_json_line(&json!({
            "type": "agent_settled"
        })))
        .await
        .expect("settled JSON event should be written");
        while let Ok(line) = written_rx.try_recv() {
            received.push(serde_json::from_str(&line).expect("valid trailing JSON event"));
        }

        let event_types: Vec<_> = received
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect();
        assert_eq!(
            event_types,
            vec![
                "agent_start",
                "turn_start",
                "message_start",
                "message_end",
                "message_start",
                "message_update",
                "message_update",
                "message_update",
                "message_end",
                "turn_end",
                "agent_end",
                "agent_settled",
            ]
        );
    }
}
