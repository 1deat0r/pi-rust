//! RPC mode — port of `packages/coding-agent/src/modes/rpc/rpc-mode.ts`.
//!
//! Headless operation over a JSONL stdin/stdout protocol. Receives `RpcCommand`
//! objects as one JSON per line on stdin; emits `response` records and
//! `message_update`/`agent_settled` events as JSON lines on stdout.
//!
//! Implemented over the port's current layers: the pi-ai Models facade
//! (provider registry / catalog / auth + stream dispatch), the pi-agent agent
//! loop (with stream-event observation), and the session facade (JSONL v4
//! repo-backed). Commands whose upstream dependency is not yet ported (HTML
//! export, extension commands) respond with the upstream error surface.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::agent::{run_agent_loop, AgentContext, AgentLoopConfig};
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::session::Session as JsonlSession;

use pi_agent::session::state::{EntryQuery, ForkOptions};
use pi_agent::session::types::{EntryNoStats, SessionMetadata};
use pi_agent::session::JsonlSessionRepo;
use pi_ai::model::Model;
use pi_ai::models::Models;
use pi_ai::types::{AssistantMessageEvent, Message, UserContent};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

use super::jsonl::{serialize_json_line, JsonlLineReader};
use super::rpc_types::{failure, success, RpcCommand, RpcSessionState};

/// Max output chars before a bash result is truncated (upstream threshold).
const BASH_TRUNCATE_LIMIT: usize = 30_000;


/// The RPC runtime: owns the current model/session and executes commands.
pub struct RpcRuntime {
    pub cwd: String,
    pub agent_dir: String,
    pub settings: SettingsManager,
    pub models: Models,
    pub provider: String,
    pub model: Model,
    pub thinking_level: pi_ai::types::ModelThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub session_root: String,
    pub session_path: Option<String>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub auto_retry_enabled: bool,
    pub messages: Vec<pi_agent::types::AgentMessage>,
    pub repo: JsonlSessionRepo<pi_agent::fs::StdFileSystem>,
    pub session: JsonlSession<pi_agent::fs::StdFileSystem>,
    pub run_lock: Arc<Mutex<bool>>,
    pub abort_bash: Arc<AtomicBool>,
    pub system_prompt: Option<String>,
    pub tools_enabled: bool,
}

impl RpcRuntime {
    /// Build a fresh runtime (mirrors the run path's model/session wiring).
    pub async fn new(args: &Args, settings: SettingsManager) -> Result<Self, String> {
        let cwd = config::cwd();
        let agent_dir = config::get_agent_dir().display().to_string();
        let models = pi_ai::providers::builtin_models(pi_ai::models::CreateModelsOptions::default());

        let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
        let model_hint = crate::run::resolve_run_model(
            args.model.as_deref(),
            &settings,
            !crate::run::has_explicit_provider(args.provider.as_deref()),
        );
        if models.get_provider(&provider).is_none() && provider != "faux" {
            return Err(format!("provider {provider:?} is not registered in the model registry"));
        }
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
        let session_id = args
            .session_id
            .clone()
            .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
            .unwrap_or_else(pi_agent::session::new_id);
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
        let meta = session.get_metadata().await;
        let session_path = Some(meta.path.clone());

        let system_prompt = args.system_prompt.clone();
        Ok(Self {
            cwd,
            agent_dir,
            settings,
            models,
            provider,
            model,
            thinking_level: pi_ai::types::ModelThinkingLevel::Off,
            is_streaming: false,
            is_compacting: false,
            steering_mode: "all".to_string(),
            follow_up_mode: "all".to_string(),
            session_root,
            session_path,
            session_id,
            session_name: None,
            auto_compaction_enabled: true,
            auto_retry_enabled: true,
            messages: Vec::new(),
            repo,
            session,
            run_lock: Arc::new(Mutex::new(false)),
            abort_bash: Arc::new(AtomicBool::new(false)),
            system_prompt,
            tools_enabled: !args.no_tools,
        })
    }

    /// Available models snapshot (all catalog models across providers).
    fn available_models(&self) -> Vec<Model> {
        self.models.get_models(None)
    }

    /// The stream function used by the agent loop (facade-backed dispatch;
    /// faux has its scripted path echoing the prompt).
    fn make_stream_fn(&self, reply: &str) -> crate::run::StreamFn {
        let models = self.models.clone();
        let provider = self.provider.clone();
        let api_key = std::env::var(config::ENV_KEY).ok();
        let stream_options = pi_ai::types::StreamOptions {
            base: pi_ai::types::ProviderRequestOptions {
                api_key,
                ..Default::default()
            },
            ..Default::default()
        };
        if provider == "faux" {
            let core = pi_ai::providers::FauxProviderCore::new(&pi_ai::providers::RegisterFauxProviderOptions::default());
            let reply = if reply.is_empty() { "Hello from pi-rust".to_string() } else { reply.to_string() };
            core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
                pi_ai::providers::faux_assistant_message(
                    vec![pi_ai::types::ContentBlock::text(format!("faux response to: {reply}"))],
                    pi_ai::providers::FauxAssistantOptions::default(),
                ),
            )]);
            return Arc::new(move |model, ctx| core.stream(model, ctx, None));
        }
        Arc::new(move |model, ctx| models.stream(model, ctx, Some(&stream_options)))
    }

    async fn persist_messages(&mut self, new_messages: &[pi_agent::types::AgentMessage]) -> Result<(), String> {
        for message in new_messages {
            self.session
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
        Ok(())
    }

    /// Serialize all entries in the current session (oldest-first).
    async fn get_entries(&self) -> Result<Vec<pi_agent::session::types::Entry>, String> {
        self.session
            .find_entries(&EntryQuery {
                order: Some(pi_agent::session::state::EntryOrder::OldestFirst),
                id: None,
                entry_type: None,
                custom_type: None,
                cursor: None,
                limit: None,
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Build an RPC session-tree: parent-linked entries (upstream
    /// SessionTreeNode: entry + children + optional label).
    fn build_tree(entries: &[pi_agent::session::types::Entry]) -> serde_json::Value {
        let mut nodes: Vec<serde_json::Value> = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        for entry in entries {
            let node = serde_json::json!({ "entry": entry, "children": [] });
            by_id.insert(entry.id().to_string(), nodes.len());
            nodes.push(node);
        }
        let mut roots: Vec<serde_json::Value> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            match entry.parent_id() {
                Some(parent) if by_id.contains_key(parent) => {
                    let parent_idx = by_id[parent];
                    let child = nodes[i].clone();
                    nodes[parent_idx]["children"]
                        .as_array_mut()
                        .unwrap()
                        .push(child);
                }
                _ => roots.push(nodes[i].clone()),
            }
        }
        serde_json::Value::Array(roots)
    }

    fn build_session_state(&self) -> RpcSessionState {
        RpcSessionState {
            model: Some(serde_json::to_value(&self.model).unwrap_or(serde_json::Value::Null)),
            thinking_level: self.thinking_level.as_str().to_string(),
            is_streaming: self.is_streaming,
            is_compacting: self.is_compacting,
            steering_mode: self.steering_mode.clone(),
            follow_up_mode: self.follow_up_mode.clone(),
            session_file: self.session_path.clone(),
            session_id: self.session_id.clone(),
            session_name: self.session_name.clone(),
            auto_compaction_enabled: self.auto_compaction_enabled,
            message_count: self.messages.len(),
            pending_message_count: 0,
        }
    }

    /// Execute a single parsed command; returns the JSON lines to write
    /// (responses + streamed events).
    pub async fn handle_command(&mut self, command: RpcCommand, store: &mut Vec<String>) -> Result<(), String> {
        let id = command.id.clone();
        let cmd = command.type_.clone();
        let respond = |store: &mut Vec<String>, value: serde_json::Value| {
            store.push(serialize_json_line(&value));
        };
        let fail = |store: &mut Vec<String>, id: &Option<String>, cmd: &str, msg: String| {
            store.push(serialize_json_line(&failure(id.as_deref(), cmd, msg)));
        };

        match command.type_.as_str() {
            // =================================================================
            // Prompting
            // =================================================================
            "prompt" | "steer" | "follow_up" => {
                let Some(message) = command.str_field("message") else {
                    fail(store, &id, &cmd, "missing message".to_string());
                    return Ok(());
                };
                {
                    let mut lock = self.run_lock.lock().unwrap();
                    if *lock {
                        fail(store, &id, &cmd, "Agent is already streaming; send abort first".to_string());
                        return Ok(());
                    }
                    *lock = true;
                }
                self.is_streaming = true;
                // Preflight success response is emitted first (upstream
                // preflightResult).
                respond(store, success(id.as_deref(), &cmd, None));

                let prompt = pi_agent::agent::user_text_prompt(message.clone(), pi_ai::types::now_ms());
                let prompts = vec![prompt.clone()];
                self.messages.push(prompt.clone());

                // Persist the user message.
                let _ = self.persist_messages(&[prompt.clone()]).await;

                // Stream events.
                let mut agent_context = AgentContext::new(self.system_prompt.clone(), Vec::new());
                if self.tools_enabled {
                    agent_context.tools.push(pi_agent::tools::bash_tool(self.cwd.clone()));
                    agent_context.tools.push(pi_agent::tools::read_tool(self.cwd.clone()));
                    agent_context.tools.push(pi_agent::tools::write_tool(self.cwd.clone()));
                    agent_context.tools.push(pi_agent::tools::edit_tool(self.cwd.clone()));
                    agent_context.tools.push(crate::core::tools::ls_tool(self.cwd.clone()));
                    agent_context.tools.push(crate::core::tools::find_tool(self.cwd.clone()));
                    agent_context.tools.push(crate::core::tools::grep_tool(self.cwd.clone()));
                }
                let stream_fn = self.make_stream_fn(&message);

                // Events are captured into a shared sink (the observer is
                // `Fn`, so the sink must be interior-mutable).
                let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let observer: Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync> = {
                    let sink = events.clone();
                    Arc::new(move |event: &AssistantMessageEvent| {
                        let update = to_json_message_update(event);
                        sink.lock().unwrap().push(serialize_json_line(&update));
                    })
                };
                let cfg = AgentLoopConfig {
                    model: self.model.clone(),
                    stream_fn,
                    signal: None,
                    stop_after_turn: true,
                    on_stream_event: Some(observer),
                };
                let new_messages = run_agent_loop(prompts, &mut agent_context, &cfg, &mut |_| {}).await;

                // Emit the captured stream events in wire order.
                let captured_events = events.lock().unwrap().drain(..).collect::<Vec<String>>();
                for line in captured_events {
                    store.push(line);
                }

                // Fold the assistant (and any tool results) into the runtime state.
                let mut persisted: Vec<pi_agent::types::AgentMessage> = Vec::new();
                for m in new_messages.iter().skip(1) {
                    self.messages.push(m.clone());
                    persisted.push(m.clone());
                }
                let _ = self.persist_messages(&persisted).await;

                self.is_streaming = false;
                {
                    let mut lock = self.run_lock.lock().unwrap();
                    *lock = false;
                }
                store.push(serialize_json_line(&serde_json::json!({"type": "agent_settled"})));
                Ok(())
            }

            "abort" => {
                self.is_streaming = false;
                let mut lock = self.run_lock.lock().unwrap();
                *lock = false;
                self.abort_bash.store(true, Ordering::SeqCst);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "new_session" => {
                let parent = command.str_field("parentSession");
                let session_id = pi_agent::session::new_id();
                let session = self
                    .repo
                    .create(CreateOptions {
                        id: Some(session_id.clone()),
                        cwd: self.cwd.clone(),
                        parent_session_id: parent,
                        metadata: None,
                        fork_options: ForkOptions::Tree,
                    })
                    .await
                    .map_err(|e| {
                        let msg = format!("create session: {e}");
                        fail(store, &id, &cmd, msg);
                        e.to_string()
                    })?;
                let meta = session.get_metadata().await;
                self.session_path = Some(meta.path.clone());
                self.session_id = session_id.clone();
                self.session_name = None;
                self.messages.clear();
                self.session = session;
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({"cancelled": false}))));
                Ok(())
            }

            // =================================================================
            // State
            // =================================================================
            "get_state" => {
                let state = self.build_session_state();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::to_value(state).unwrap_or(serde_json::Value::Null))));
                Ok(())
            }

            // =================================================================
            // Model
            // =================================================================
            "set_model" => {
                let provider_name = command.str_field("provider").ok_or_else(|| "missing provider".to_string())?;
                let model_id = command.str_field("modelId").ok_or_else(|| "missing modelId".to_string())?;
                let model = self.models.get_model(&provider_name, &model_id);
                match model {
                    Some(model) => {
                        self.provider = provider_name.clone();
                        self.model = model.clone();
                        respond(store, success(id.as_deref(), &cmd, Some(serde_json::to_value(model).unwrap_or(serde_json::Value::Null))));
                        Ok(())
                    }
                    None => {
                        fail(store, &id, &cmd, format!("Model not found: {provider_name}/{model_id}"));
                        Ok(())
                    }
                }
            }

            "cycle_model" => {
                let available = self.available_models();
                let current = available.iter().position(|m| m.provider == self.model.provider && m.id == self.model.id);
                match current {
                    Some(idx) if !available.is_empty() => {
                        let next = available[(idx + 1) % available.len()].clone();
                        self.provider = next.provider.clone();
                        self.model = next.clone();
                        let data = serde_json::json!({
                            "model": next,
                            "thinkingLevel": self.thinking_level.as_str(),
                            "isScoped": false,
                        });
                        respond(store, success(id.as_deref(), &cmd, Some(data)));
                        Ok(())
                    }
                    _ => {
                        respond(store, success(id.as_deref(), &cmd, Some(serde_json::Value::Null)));
                        Ok(())
                    }
                }
            }

            "get_available_models" => {
                let models = self.available_models();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({"models": models}))));
                Ok(())
            }

            // =================================================================
            // Thinking
            // =================================================================
            "set_thinking_level" => {
                let level = command.str_field("level").unwrap_or_else(|| "off".to_string());
                let parsed = level.parse::<pi_ai::types::ModelThinkingLevel>().unwrap_or(pi_ai::types::ModelThinkingLevel::Off);
                self.thinking_level = parsed;
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "cycle_thinking_level" => {
                let available = pi_ai::model::get_supported_thinking_levels(&self.model);
                if available.is_empty() {
                    respond(store, success(id.as_deref(), &cmd, Some(serde_json::Value::Null)));
                    return Ok(());
                }
                let current = available.iter().position(|l| *l == self.thinking_level);
                let next_idx = match current {
                    Some(idx) => (idx + 1) % available.len(),
                    None => 0,
                };
                self.thinking_level = available[next_idx];
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "level": self.thinking_level.as_str() }))));
                Ok(())
            }

            "get_available_thinking_levels" => {
                let levels = pi_ai::model::get_supported_thinking_levels(&self.model)
                    .into_iter()
                    .map(|l| l.as_str().to_string())
                    .collect::<Vec<_>>();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "levels": levels }))));
                Ok(())
            }

            // =================================================================
            // Queue modes
            // =================================================================
            "set_steering_mode" => {
                let mode = command.str_field("mode").unwrap_or_else(|| "all".to_string());
                self.steering_mode = mode;
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "set_follow_up_mode" => {
                let mode = command.str_field("mode").unwrap_or_else(|| "all".to_string());
                self.follow_up_mode = mode;
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            // =================================================================
            // Compaction / retry
            // =================================================================
            "set_auto_compaction" => {
                self.auto_compaction_enabled = command.bool_field("enabled").unwrap_or(true);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "set_auto_retry" => {
                self.auto_retry_enabled = command.bool_field("enabled").unwrap_or(true);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "abort_retry" => {
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "compact" => {
                // Compaction wiring over the harness is a P8 follow-up; the
                // upstream command is only meaningful with auto-compaction
                // services bound.
                fail(store, &id, &cmd, "compact is not supported in this build (compaction runtime wiring pending)".to_string());
                Ok(())
            }

            // =================================================================
            // Bash
            // =================================================================
            "bash" => {
                let bash_command = command.str_field("command").ok_or_else(|| "missing command".to_string())?;
                self.abort_bash.store(false, Ordering::SeqCst);
                let result = run_bash(&bash_command, &self.cwd, self.abort_bash.clone()).await;
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)),
                    ),
                );
                Ok(())
            }

            "abort_bash" => {
                self.abort_bash.store(true, Ordering::SeqCst);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            // =================================================================
            // Session
            // =================================================================
            "get_session_stats" => {
                let stats = self.get_entries().await.map_err(|e| {
                    fail(store, &id, &cmd, e.clone());
                    e
                })?;
                let user_messages = stats.iter().filter(|e| e.as_message().is_some_and(|m| matches!(m, pi_agent::types::AgentMessage::Core(Message::User(_))))).count();
                let assistant_messages = stats.iter().filter(|e| e.as_message().is_some_and(|m| matches!(m, pi_agent::types::AgentMessage::Core(Message::Assistant(_))))).count();
                let tool_calls = stats.iter().filter(|e| e.as_message().is_some_and(|m| matches!(m, pi_agent::types::AgentMessage::Core(Message::Assistant(a)) if a.content().iter().any(|b| matches!(b, pi_ai::types::ContentBlock::ToolCall { .. }))))).count();
                let tool_results = stats.iter().filter(|e| e.as_message().is_some_and(|m| matches!(m, pi_agent::types::AgentMessage::Core(Message::ToolResult(_))))).count();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({
                    "sessionFile": self.session_path,
                    "sessionId": self.session_id,
                    "userMessages": user_messages,
                    "assistantMessages": assistant_messages,
                    "toolCalls": tool_calls,
                    "toolResults": tool_results,
                    "totalMessages": user_messages + assistant_messages + tool_calls + tool_results,
                    "tokens": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
                    "cost": 0,
                }))));
                Ok(())
            }

            "export_html" => {
                fail(store, &id, &cmd, "export_html is not supported in this build (export-html port pending)".to_string());
                Ok(())
            }

            "switch_session" => {
                let session_path = command.str_field("sessionPath").ok_or_else(|| "missing sessionPath".to_string())?;
                match self.load_session(&session_path).await {
                    Ok(()) => {
                        respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({"cancelled": false}))));
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e);
                        Ok(())
                    }
                }
            }

            "fork" => {
                let entry_id = command.str_field("entryId").ok_or_else(|| "missing entryId".to_string())?;
                match self.fork_session(Some(entry_id)).await {
                    Ok(_) => {
                        respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({"text": "", "cancelled": false}))));
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e);
                        Ok(())
                    }
                }
            }

            "clone" => {
                match self.fork_session(None).await {
                    Ok(_) => {
                        respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({"cancelled": false}))));
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e);
                        Ok(())
                    }
                }
            }

            "get_fork_messages" => {
                let messages: Vec<serde_json::Value> = self
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        pi_agent::types::AgentMessage::Core(Message::User(_)) => {
                            let text = pi_agent::agent::user_content_text(&user_content_of(m));
                            Some(serde_json::json!({ "entryId": "-", "text": text }))
                        }
                        _ => None,
                    })
                    .collect();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "messages": messages }))));
                Ok(())
            }

            "get_entries" => {
                let mut entries = self.get_entries().await.map_err(|e| {
                    fail(store, &id, &cmd, e.clone());
                    e
                })?;
                if let Some(since) = command.str_field("since") {
                    let since_index = entries.iter().position(|e| e.id() == since);
                    match since_index {
                        Some(idx) => {
                            entries = entries.split_off(idx + 1);
                        }
                        None => {
                            fail(store, &id, &cmd, format!("Entry not found: {since}"));
                            return Ok(());
                        }
                    }
                }
                let leaf_id = self.session.get_leaf_id().await.ok().flatten();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "entries": entries, "leafId": leaf_id }))));
                Ok(())
            }

            "get_tree" => {
                let entries = self.get_entries().await.map_err(|e| {
                    fail(store, &id, &cmd, e.clone());
                    e
                })?;
                let tree = Self::build_tree(&entries);
                let leaf_id = self.session.get_leaf_id().await.ok().flatten();
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "tree": tree, "leafId": leaf_id }))));
                Ok(())
            }

            "get_last_assistant_text" => {
                let text = self
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        pi_agent::types::AgentMessage::Core(Message::Assistant(a)) => {
                            let parts: Vec<String> = a
                                .content()
                                .iter()
                                .filter_map(|b| match b {
                                    pi_ai::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                                    _ => None,
                                })
                                .collect();
                            if parts.is_empty() { None } else { Some(parts.join("")) }
                        }
                        _ => None,
                    });
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "text": text }))));
                Ok(())
            }

            "set_session_name" => {
                let name = command.str_field("name").unwrap_or_default().trim().to_string();
                if name.is_empty() {
                    fail(store, &id, &cmd, "Session name cannot be empty".to_string());
                    return Ok(());
                }
                self.session.set_name(Some(&name)).await.map_err(|e| {
                    fail(store, &id, &cmd, e.to_string());
                    e.to_string()
                })?;
                self.session_name = Some(name);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            // =================================================================
            // Messages / commands
            // =================================================================
            "get_messages" => {
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "messages": self.messages }))));
                Ok(())
            }

            "get_commands" => {
                respond(store, success(id.as_deref(), &cmd, Some(serde_json::json!({ "commands": [] }))));
                Ok(())
            }

            other => {
                fail(store, &id, other, format!("Unknown command: {other}"));
                Ok(())
            }
        }
    }

    async fn load_session(&mut self, path: &str) -> Result<(), String> {
        let metadata = SessionMetadata {
            id: "loaded".to_string(),
            created_at: 0,
            cwd: self.cwd.clone(),
            path: path.to_string(),
            modified_at: 0,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };
        let session = self
            .repo
            .open(&metadata)
            .await
            .map_err(|e| format!("failed to open session {path:?}: {e}"))?;
        let meta = session.get_metadata().await;
        self.session = session;
        self.session_path = Some(meta.path);
        self.session_name = self.session.get_name().await;
        self.messages.clear();
        Ok(())
    }

    async fn fork_session(&mut self, entry_id: Option<String>) -> Result<(), String> {
        let metadata = SessionMetadata {
            id: self.session_id.clone(),
            created_at: 0,
            cwd: self.cwd.clone(),
            path: self
                .session_path
                .clone()
                .unwrap_or_else(|| "".to_string()),
            modified_at: 0,
            source_format: 4,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        };
        let fork_options = match &entry_id {
            Some(entry_id) => ForkOptions::Branch {
                entry_id: Some(entry_id.clone()),
                position: None,
            },
            None => ForkOptions::Branch {
                entry_id: None,
                position: None,
            },
        };
        let session = self
            .repo
            .fork(
                &metadata,
                CreateOptions {
                    id: Some(pi_agent::session::new_id()),
                    cwd: self.cwd.clone(),
                    parent_session_id: None,
                    metadata: None,
                    fork_options,
                },
            )
            .await
            .map_err(|e| format!("fork failed: {e}"))?;
        let meta = session.get_metadata().await;
        self.session = session;
        self.session_path = Some(meta.path);
        self.session_id = pi_agent::session::new_id();
        self.session_name = None;
        self.messages.clear();
        Ok(())
    }
}

fn user_content_of(m: &pi_agent::types::AgentMessage) -> &UserContent {
    match m {
        pi_agent::types::AgentMessage::Core(Message::User(u)) => u,
        _ => panic!("expected user message"),
    }
}

/// Run a bash command synchronously, capturing combined output
/// (upstream `BashResult` shape).
pub async fn run_bash(command: &str, cwd: &str, abort: Arc<AtomicBool>) -> serde_json::Value {
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    let exit_code = match tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            if let Some(mut stdout) = stdout {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 4096];
                loop {
                    if abort.load(Ordering::SeqCst) {
                        let _ = child.kill().await;
                        break;
                    }
                    let n = match stdout.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => 0,
                    };
                    if n == 0 {
                        break;
                    }
                    if out.len() < BASH_TRUNCATE_LIMIT {
                        out.extend_from_slice(&buf[..n]);
                        if out.len() >= BASH_TRUNCATE_LIMIT {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
            }
            if let Some(mut stderr) = stderr {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 4096];
                loop {
                    if abort.load(Ordering::SeqCst) {
                        let _ = child.kill().await;
                        break;
                    }
                    let n = match stderr.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => 0,
                    };
                    if n == 0 {
                        break;
                    }
                    if out.len() < BASH_TRUNCATE_LIMIT {
                        out.extend_from_slice(&buf[..n]);
                        if out.len() >= BASH_TRUNCATE_LIMIT {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
            }
            match child.wait().await {
                Ok(status) => status.code(),
                Err(_) => None,
            }
        }
        Err(_) => None,
    };
    let output = String::from_utf8_lossy(&out).into_owned();
    serde_json::json!({
        "output": output,
        "exitCode": exit_code,
        "cancelled": abort.load(Ordering::SeqCst),
        "truncated": truncated,
    })
}

/// Convert an assistant stream event to the JSON `message_update` wire form
/// (upstream `toJsonEvent`): cumulative `partial` snapshots are stripped;
/// `toolcall_start` carries id + toolName.
pub fn to_json_message_update(event: &AssistantMessageEvent) -> serde_json::Value {
    let usage = event.partial().and_then(|p| p.usage()).cloned();
    let (kind, mut body) = event_json(event);
    let usage = usage
        .map(|u| serde_json::to_value(u))
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(serde_json::Value::Null);
    body.insert("type".to_string(), serde_json::json!("message_update"));
    body.insert("usage".to_string(), usage);
    body.insert("assistantMessageEvent".to_string(), serde_json::Value::Object(kind));
    serde_json::Value::Object(body)
}

fn event_json(event: &AssistantMessageEvent) -> (serde_json::Map<String, serde_json::Value>, serde_json::Map<String, serde_json::Value>) {
    match event {
        AssistantMessageEvent::Start { partial } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("message_start"));
            m.insert("message".into(), serde_json::to_value(partial).unwrap_or(serde_json::Value::Null));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::TextStart { content_index, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("text_start"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::TextDelta { content_index, delta, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("text_delta"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("delta".into(), serde_json::json!(delta));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::TextEnd { content_index, content, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("text_end"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("content".into(), serde_json::json!(content));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ThinkingStart { content_index, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("thinking_start"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ThinkingDelta { content_index, delta, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("thinking_delta"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("delta".into(), serde_json::json!(delta));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ThinkingEnd { content_index, content, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("thinking_end"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("content".into(), serde_json::json!(content));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ToolCallStart { content_index, partial, .. } => {
            let (id, tool_name) = partial
                .content()
                .get(*content_index)
                .map(|b| match b {
                    pi_ai::types::ContentBlock::ToolCall { id, name, .. } => (id.clone(), Some(name.clone())),
                    _ => (String::new(), None),
                })
                .unwrap_or_default();
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("toolcall_start"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("id".into(), serde_json::json!(id));
            if let Some(name) = tool_name {
                m.insert("toolName".into(), serde_json::json!(name));
            }
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ToolCallDelta { content_index, delta, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("toolcall_delta"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("delta".into(), serde_json::json!(delta));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ToolCallEnd { content_index, tool_call, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("toolcall_end"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("toolCall".into(), serde_json::to_value(tool_call).unwrap_or(serde_json::Value::Null));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::Done { reason, message } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("done"));
            let reason_str = match reason {
                pi_ai::types::DoneReason::Stop => "stop",
                pi_ai::types::DoneReason::Length => "length",
                pi_ai::types::DoneReason::ToolUse => "toolUse",
                pi_ai::types::DoneReason::Deferred => "deferred",
            };
            m.insert("reason".into(), serde_json::json!(reason_str));
            m.insert("message".into(), serde_json::to_value(message).unwrap_or(serde_json::Value::Null));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::Error { reason, error_message } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("error"));
            let reason_str = match reason {
                pi_ai::types::ErrorReason::Aborted => "aborted",
                pi_ai::types::ErrorReason::Error => "error",
            };
            m.insert("reason".into(), serde_json::json!(reason_str));
            m.insert("message".into(), serde_json::to_value(error_message).unwrap_or(serde_json::Value::Null));
            (m, serde_json::Map::new())
        }
    }
}

/// Run the RPC mode loop: read commands from stdin, write responses/events
/// to stdout until EOF.
pub async fn run_rpc_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut runtime = RpcRuntime::new(args, settings).await?;
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = JsonlLineReader::new(stdin);
    use tokio::io::AsyncWriteExt;
    let mut out = tokio::io::BufWriter::new(stdout);

    loop {
        let Some(line) = reader.next_line().await.map_err(|e| format!("stdin read error: {e}"))? else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = failure(None, "parse", format!("Failed to parse command: {e}"));
                super::jsonl::write_json_line(&mut out, serialize_json_line(&resp)).await.map_err(|e| e.to_string())?;
                out.flush().await.map_err(|e| e.to_string())?;
                continue;
            }
        };
        let command = match RpcCommand::parse(parsed) {
            Ok(c) => c,
            Err(e) => {
                let resp = failure(None, "parse", e);
                super::jsonl::write_json_line(&mut out, serialize_json_line(&resp)).await.map_err(|e| e.to_string())?;
                out.flush().await.map_err(|e| e.to_string())?;
                continue;
            }
        };
        let mut store: Vec<String> = Vec::new();
        runtime.handle_command(command, &mut store).await?;
        for line in store {
            super::jsonl::write_json_line(&mut out, line).await.map_err(|e| e.to_string())?;
        }
        out.flush().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn runtime_for_test() -> RpcRuntime {
        let args = crate::args::parse_args(&["--provider".to_string(), "faux".to_string(), "--no-tools".to_string()])
            .expect_run();
        let settings = SettingsManager::in_memory(Default::default());
        RpcRuntime::new(&args, settings).await.unwrap()
    }

    #[test]
    fn to_json_event_strips_partial() {
        let mut partial = pi_ai::types::AssistantMessage::new();
        partial.set_api_provider_model("faux", "faux", "faux-1");
        partial.set_usage(pi_ai::types::Usage::default());
        let event = AssistantMessageEvent::TextDelta { content_index: 0, delta: "hi".into(), partial: partial.clone() };
        let json = to_json_message_update(&event);
        assert_eq!(json["type"], "message_update");
        assert_eq!(json["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(json["assistantMessageEvent"]["delta"], "hi");
        assert!(json["assistantMessageEvent"].get("partial").is_none());
    }

    #[tokio::test]
    async fn get_state_returns_shape() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(), &mut store)
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        let line = store[0].trim();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "response");
        assert_eq!(v["command"], "get_state");
        assert_eq!(v["success"], true);
        assert!(v["data"]["sessionId"].is_string(), "data was: {}", v["data"]);
    }

    #[tokio::test]
    async fn set_model_unknown_errors() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "set_model", "provider": "google", "modelId": "nope"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("Model not found"));
    }

    #[tokio::test]
    async fn set_model_known_succeeds() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "set_model", "provider": "google", "modelId": "gemini-2.5-flash"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["id"], "gemini-2.5-flash");
    }

    #[tokio::test]
    async fn thinking_levels_roundtrip() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "set_thinking_level", "level": "high"})).unwrap(), &mut store)
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["data"]["thinkingLevel"], "high");
    }

    #[tokio::test]
    async fn bash_executes_and_captures() {
        let abort = Arc::new(AtomicBool::new(false));
        let result = run_bash("echo hello-rpc", "/tmp", abort).await;
        assert_eq!(result["output"], "hello-rpc\n");
        assert_eq!(result["exitCode"], 0);
    }

    #[tokio::test]
    async fn session_name_set_and_get() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "set_session_name", "name": "my session"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], true);
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["data"]["sessionName"], "my session");
    }

    #[tokio::test]
    async fn prompt_streams_events_and_settles() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hello"})).unwrap(), &mut store)
            .await
            .unwrap();
        // First line: success response; then message_update events; last: agent_settled.
        assert!(store.len() >= 2);
        let first: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(first["type"], "response");
        assert_eq!(first["command"], "prompt");
        assert_eq!(first["success"], true);
        let last: serde_json::Value = serde_json::from_str(store.last().unwrap().trim()).unwrap();
        assert_eq!(last["type"], "agent_settled");
        // At least one message_update event was streamed for the faux reply.
        let has_update = store.iter().any(|l| {
            serde_json::from_str::<serde_json::Value>(l.trim())
                .map(|v| v["type"] == "message_update")
                .unwrap_or(false)
        });
        assert!(has_update, "expected message_update events");
    }

    #[tokio::test]
    async fn get_messages_after_prompt() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hi"})).unwrap(), &mut store)
            .await
            .unwrap();
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "get_messages"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        let messages = v["data"]["messages"].as_array().unwrap();
        assert!(messages.len() >= 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[tokio::test]
    async fn get_entries_returns_persisted_entries() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hello"})).unwrap(), &mut store)
            .await
            .unwrap();
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "get_entries"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        let entries = v["data"]["entries"].as_array().unwrap();
        assert!(entries.len() >= 2);
        assert_eq!(entries[0]["type"], "message");
    }

    #[tokio::test]
    async fn unknown_command_errors() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(RpcCommand::parse(serde_json::json!({"type": "nope"})).unwrap(), &mut store)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("Unknown command: nope"));
    }
}
