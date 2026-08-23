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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::agent::AgentContext;
use pi_agent::rich_agent::{
    run_rich_agent_loop, PendingMessageQueue, QueueMode, RichAgentEvent, RichAgentLoopConfig,
};
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::session::Session as JsonlSession;

use pi_agent::harness::{BoxFuture, CompleteSimpleFn, SimpleModels};
use pi_agent::session::state::{BranchBounds, EntryOrder, EntryQuery, ForkOptions};
use pi_agent::session::types::{EntryNoStats, SessionMetadata};
use pi_agent::session::JsonlSessionRepo;
use pi_ai::model::Model;
use pi_ai::models::Models;
use pi_ai::types::AssistantMessage;
use pi_ai::types::SimpleStreamOptions;
use pi_ai::types::{AssistantMessageEvent, DoneReason, Message};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::args::Args;
use crate::config;
use crate::core::settings::SettingsManager;

use super::jsonl::{serialize_json_line, JsonlLineReader};
use super::rpc_types::{failure, success, RpcCommand, RpcSessionState};

/// Always-resolvable API-key auth for the scripted faux provider. The real
/// faux core ignores the key; this exists so facade-backed paths
/// (e.g. RPC compact summary generation) pass `apply_auth` like any provider.
struct FauxApiKeyAuth;

impl pi_ai::auth::ApiKeyAuth for FauxApiKeyAuth {
    fn name(&self) -> &str {
        "Faux API key"
    }
    fn check(
        &self,
        _ctx: &pi_ai::auth::AuthContext,
        _credential: Option<&pi_ai::auth::ApiKeyCredential>,
    ) -> Option<pi_ai::auth::AuthCheck> {
        Some(pi_ai::auth::AuthCheck {
            source: Some("faux".to_string()),
            auth_type: "api_key",
        })
    }
    fn resolve(
        &self,
        _ctx: &pi_ai::auth::AuthContext,
        _credential: Option<&pi_ai::auth::ApiKeyCredential>,
    ) -> Option<pi_ai::auth::AuthResult> {
        Some(pi_ai::auth::AuthResult {
            auth: pi_ai::auth::ModelAuth {
                api_key: Some("faux-key".to_string()),
                headers: None,
                base_url: None,
            },
            env: None,
            source: Some("faux".to_string()),
        })
    }
}

/// Max output chars before a bash result is truncated (upstream threshold).
const BASH_TRUNCATE_LIMIT: usize = 30_000;

fn queue_mode(value: &str) -> QueueMode {
    if value == "all" {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

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
    pub abort_signal: Arc<AtomicBool>,
    pub abort_retry_signal: Arc<AtomicBool>,
    pub steering_queue: Arc<Mutex<PendingMessageQueue>>,
    pub follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    pub system_prompt: Option<String>,
    pub tools_enabled: bool,
}

/// Everything a prompt worker needs after it has been detached from the
/// mutable RPC runtime. The session and runtime remain available to the input
/// loop while this run is streaming.
struct RpcPromptRun {
    prompts: Vec<pi_agent::types::AgentMessage>,
    context: AgentContext,
    config: RichAgentLoopConfig,
}

struct RpcPromptResult {
    /// Messages that remain in the live agent context after retry handling.
    /// Intermediate retry failures are intentionally absent.
    new_messages: Vec<pi_agent::types::AgentMessage>,
    /// Every message-end record from the run, including intermediate retry
    /// failures. These are the durable session history.
    persisted_messages: Vec<pi_agent::types::AgentMessage>,
}

enum RpcPromptTaskMessage {
    Event(String),
    Finished(RpcPromptResult),
}

fn serialize_rpc_prompt_event(event: RichAgentEvent) -> Option<String> {
    match event {
        RichAgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => Some(serialize_json_line(&serde_json::json!({
            "type": "auto_retry_start",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        }))),
        RichAgentEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => {
            let mut event = serde_json::json!({
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
            });
            if let Some(final_error) = final_error {
                event["finalError"] = serde_json::Value::String(final_error);
            }
            Some(serialize_json_line(&event))
        }
        RichAgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => Some(serialize_json_line(&to_json_message_update(
            &assistant_message_event,
        ))),
        RichAgentEvent::MessageEnd { message } => {
            if let pi_agent::types::AgentMessage::Core(Message::Assistant(message)) = message {
                let terminal = match message.stop_reason() {
                    Some(pi_ai::types::StopReason::Error) => AssistantMessageEvent::Error {
                        reason: pi_ai::types::ErrorReason::Error,
                        error_message: message,
                    },
                    Some(pi_ai::types::StopReason::Aborted) => AssistantMessageEvent::Error {
                        reason: pi_ai::types::ErrorReason::Aborted,
                        error_message: message,
                    },
                    Some(pi_ai::types::StopReason::Length) => AssistantMessageEvent::Done {
                        reason: DoneReason::Length,
                        message,
                    },
                    Some(pi_ai::types::StopReason::ToolUse) => AssistantMessageEvent::Done {
                        reason: DoneReason::ToolUse,
                        message,
                    },
                    Some(pi_ai::types::StopReason::Deferred) => AssistantMessageEvent::Done {
                        reason: DoneReason::Deferred,
                        message,
                    },
                    _ => AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message,
                    },
                };
                Some(serialize_json_line(&to_json_message_update(&terminal)))
            } else {
                None
            }
        }
        _ => None,
    }
}

async fn run_rpc_prompt(run: RpcPromptRun, events: UnboundedSender<RpcPromptTaskMessage>) {
    let RpcPromptRun {
        prompts,
        mut context,
        config,
    } = run;
    let persisted_messages = Arc::new(Mutex::new(Vec::new()));
    let persisted_for_loop = persisted_messages.clone();
    let events_for_loop = events.clone();
    let mut emit: Box<dyn FnMut(RichAgentEvent) + Send> = Box::new(move |event| {
        if let RichAgentEvent::MessageEnd { message } = &event {
            persisted_for_loop.lock().unwrap().push(message.clone());
        }
        if let Some(line) = serialize_rpc_prompt_event(event) {
            let _ = events_for_loop.send(RpcPromptTaskMessage::Event(line));
        }
    });
    let new_messages = run_rich_agent_loop(prompts, &mut context, &config, &mut emit).await;
    let persisted_messages = std::mem::take(&mut *persisted_messages.lock().unwrap());
    let _ = events.send(RpcPromptTaskMessage::Finished(RpcPromptResult {
        new_messages,
        persisted_messages,
    }));
}

async fn write_rpc_lines<W, I>(out: &mut W, lines: I) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
    I: IntoIterator<Item = String>,
{
    for line in lines {
        super::jsonl::write_json_line(out, line)
            .await
            .map_err(|e| e.to_string())?;
    }
    out.flush().await.map_err(|e| e.to_string())
}

impl RpcRuntime {
    /// Build a fresh runtime (mirrors the run path's model/session wiring).
    pub async fn new(args: &Args, settings: SettingsManager) -> Result<Self, String> {
        let cwd = config::cwd();
        let agent_dir = config::get_agent_dir().display().to_string();
        let models = crate::core::model_registry::builtin_models();
        let steering_mode = settings.get_steering_mode().to_string();
        let follow_up_mode = settings.get_follow_up_mode().to_string();
        let auto_compaction_enabled = settings.get_compaction_enabled();
        let auto_retry_enabled = settings.get_retry_enabled();

        let provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
        let model_hint = crate::run::resolve_run_model(
            args.model.as_deref(),
            &settings,
            !crate::run::has_explicit_provider(args.provider.as_deref()),
        );
        if provider == "faux" {
            // faux is intentionally not in the builtin registry; register a
            // scripted provider so facade-backed paths (RPC compact summary
            // generation) resolve it like any real provider.
            use pi_ai::models::{
                create_provider, CreateProviderOptions, ProviderApiSpec, ProviderStreams,
            };
            use pi_ai::providers::{
                faux_assistant_message, FauxAssistantOptions, FauxProviderCore, FauxResponseStep,
                RegisterFauxProviderOptions,
            };
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(
                    "Compaction summary: history retained",
                )],
                FauxAssistantOptions::default(),
            ))]);
            let stream_core = core.clone();
            let stream = Arc::new(
                move |model: &pi_ai::model::Model,
                      ctx: &pi_ai::types::Context,
                      _options: Option<&pi_ai::types::StreamOptions>| {
                    stream_core.stream(model, ctx, None)
                },
            );
            let simple_core = core.clone();
            let stream_simple = Arc::new(
                move |model: &pi_ai::model::Model,
                      ctx: &pi_ai::types::Context,
                      options: Option<&pi_ai::types::SimpleStreamOptions>| {
                    simple_core.stream(model, ctx, options)
                },
            );
            models.set_provider(create_provider(CreateProviderOptions {
                id: "faux".to_string(),
                name: Some("Faux".to_string()),
                base_url: None,
                headers: None,
                auth: pi_ai::auth::ProviderAuth {
                    api_key: Some(std::sync::Arc::new(FauxApiKeyAuth)),
                    oauth: None,
                },
                models: core.models.clone(),
                api: ProviderApiSpec::Single(ProviderStreams {
                    stream,
                    stream_simple,
                    fetch_deferred: None,
                    cancel_deferred: None,
                }),
                filter_models: None,
            }));
        }
        if models.get_provider(&provider).is_none() {
            return Err(format!(
                "provider {provider:?} is not registered in the model registry"
            ));
        }
        let model = if provider == "faux" {
            crate::run::build_faux_model(model_hint.as_deref())?
        } else {
            crate::core::model_runtime::resolve_run_model_for_provider(
                &models,
                &provider,
                model_hint.as_deref(),
            )?
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
            steering_mode: steering_mode.clone(),
            follow_up_mode: follow_up_mode.clone(),
            session_root,
            session_path,
            session_id,
            session_name: None,
            auto_compaction_enabled,
            auto_retry_enabled,
            messages: Vec::new(),
            repo,
            session,
            run_lock: Arc::new(Mutex::new(false)),
            abort_bash: Arc::new(AtomicBool::new(false)),
            abort_signal: Arc::new(AtomicBool::new(false)),
            abort_retry_signal: Arc::new(AtomicBool::new(false)),
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(queue_mode(
                &steering_mode,
            )))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(queue_mode(
                &follow_up_mode,
            )))),
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
            let core = pi_ai::providers::FauxProviderCore::new(
                &pi_ai::providers::RegisterFauxProviderOptions::default(),
            );
            let reply = if reply.is_empty() {
                "Hello from pi-rust".to_string()
            } else {
                reply.to_string()
            };
            // Factory steps let tests observe the context the model receives
            // (e.g. multi-turn history seeding). Keep enough steps for queued
            // steering/follow-up turns to remain deterministic.
            let make_response = |reply: String| {
                let factory: Box<
                    dyn Fn(
                            &pi_ai::types::Context,
                            Option<&pi_ai::types::SimpleStreamOptions>,
                            &pi_ai::providers::FauxProviderState,
                            &pi_ai::model::Model,
                        ) -> pi_ai::types::AssistantMessage
                        + Send
                        + Sync,
                > = Box::new(
                    move |ctx: &pi_ai::types::Context,
                          _options: Option<&pi_ai::types::SimpleStreamOptions>,
                          _state: &pi_ai::providers::FauxProviderState,
                          _model: &pi_ai::model::Model| {
                        let history = ctx.messages.len();
                        pi_ai::providers::faux_assistant_message(
                            vec![pi_ai::types::ContentBlock::text(format!(
                                "faux response to: {reply} (context messages: {history})"
                            ))],
                            pi_ai::providers::FauxAssistantOptions::default(),
                        )
                    },
                );
                pi_ai::providers::FauxResponseStep::Factory(factory)
            };
            core.set_responses((0..32).map(|_| make_response(reply.clone())).collect());
            return Arc::new(move |model, ctx| core.stream(model, ctx, None));
        }
        Arc::new(move |model, ctx| models.stream(model, ctx, Some(&stream_options)))
    }

    fn prepare_prompt_run(&self, message: &str) -> RpcPromptRun {
        let prompt = pi_agent::agent::user_text_prompt(message, pi_ai::types::now_ms());
        let prompts = vec![prompt];

        // Seed the model context with prior history. The current prompt is
        // passed separately in `prompts`, matching the synchronous RPC path.
        let mut context = AgentContext::new(self.system_prompt.clone(), Vec::new());
        context.messages = self.messages.clone();
        if self.tools_enabled {
            context
                .tools
                .push(pi_agent::tools::bash_tool(self.cwd.clone()));
            context
                .tools
                .push(pi_agent::tools::read_tool(self.cwd.clone()));
            context
                .tools
                .push(pi_agent::tools::write_tool(self.cwd.clone()));
            context
                .tools
                .push(pi_agent::tools::edit_tool(self.cwd.clone()));
            context
                .tools
                .push(crate::core::tools::ls_tool(self.cwd.clone()));
            context
                .tools
                .push(crate::core::tools::find_tool(self.cwd.clone()));
            context
                .tools
                .push(crate::core::tools::grep_tool(self.cwd.clone()));
        }

        // The rich loop owns the queue drain points. Control commands can
        // therefore enqueue messages while this run is detached from the
        // mutable runtime.
        let steering_queue = self.steering_queue.clone();
        let follow_up_queue = self.follow_up_queue.clone();
        let steering_hook: pi_agent::rich_agent::AsyncHook<(), Vec<pi_agent::types::AgentMessage>> =
            Arc::new(move |_| {
                let queue = steering_queue.clone();
                Box::pin(async move { queue.lock().unwrap().drain() })
            });
        let follow_up_hook: pi_agent::rich_agent::AsyncHook<
            (),
            Vec<pi_agent::types::AgentMessage>,
        > = Arc::new(move |_| {
            let queue = follow_up_queue.clone();
            Box::pin(async move { queue.lock().unwrap().drain() })
        });
        let (settings_retry_enabled, max_retries, base_delay_ms) =
            self.settings.get_retry_settings();
        let retry_policy = (self.auto_retry_enabled && settings_retry_enabled).then_some(
            pi_ai::utils::retry::RetryPolicy {
                enabled: true,
                max_retries: max_retries as u32,
                base_delay_ms,
            },
        );
        let config = RichAgentLoopConfig {
            model: self.model.clone(),
            stream_fn: self.make_stream_fn(message),
            signal: Some(self.abort_signal.clone()),
            get_steering_messages: steering_hook,
            get_follow_up_messages: follow_up_hook,
            retry_policy,
            retry_signal: Some(self.abort_retry_signal.clone()),
            ..RichAgentLoopConfig::new(
                self.model.clone(),
                Arc::new(|_, _| pi_ai::AssistantMessageEventStream::new()),
                Some(self.abort_signal.clone()),
            )
        };

        RpcPromptRun {
            prompts,
            context,
            config,
        }
    }

    /// Start a detached prompt worker and append its preflight response to
    /// `store`. Returning `Some(receiver)` means the caller must consume the
    /// worker's ordered event/completion stream before starting another run.
    fn start_prompt_task(
        &mut self,
        command: RpcCommand,
        store: &mut Vec<String>,
    ) -> Option<UnboundedReceiver<RpcPromptTaskMessage>> {
        let id = command.id.clone();
        let cmd = command.type_.clone();
        let respond = |store: &mut Vec<String>, value: serde_json::Value| {
            store.push(serialize_json_line(&value));
        };
        let fail = |store: &mut Vec<String>, id: &Option<String>, cmd: &str, msg: String| {
            store.push(serialize_json_line(&failure(id.as_deref(), cmd, msg)));
        };

        let Some(message) = command.str_field("message") else {
            fail(store, &id, &cmd, "missing message".to_string());
            return None;
        };
        {
            let mut lock = self.run_lock.lock().unwrap();
            if *lock {
                fail(
                    store,
                    &id,
                    &cmd,
                    "Agent is already streaming; send abort first".to_string(),
                );
                return None;
            }
            *lock = true;
        }
        self.is_streaming = true;
        self.abort_signal.store(false, Ordering::SeqCst);
        self.abort_retry_signal.store(false, Ordering::SeqCst);
        respond(store, success(id.as_deref(), &cmd, None));

        let (events, receiver) = mpsc::unbounded_channel();
        let run = self.prepare_prompt_run(&message);
        tokio::spawn(run_rpc_prompt(run, events));
        Some(receiver)
    }

    async fn settle_prompt_with_persistence(
        &mut self,
        new_messages: Vec<pi_agent::types::AgentMessage>,
        persisted_messages: Vec<pi_agent::types::AgentMessage>,
    ) -> Vec<String> {
        let mut store = Vec::new();
        self.messages.extend(new_messages.iter().cloned());
        // Rich events normally cover every live message. If the run was
        // aborted before the assistant stream emitted its message lifecycle,
        // fall back to the live result so that the terminal assistant record
        // is not silently lost.
        let messages_to_persist =
            if persisted_messages.is_empty() || persisted_messages.len() < new_messages.len() {
                &new_messages
            } else {
                &persisted_messages
            };
        let _ = self.persist_messages(messages_to_persist).await;

        self.is_streaming = false;
        {
            let mut lock = self.run_lock.lock().unwrap();
            *lock = false;
        }
        if self.maybe_auto_compact().await.unwrap_or(false) {
            store.push(serialize_json_line(
                &serde_json::json!({"type": "compacted"}),
            ));
        }
        store.push(serialize_json_line(
            &serde_json::json!({"type": "agent_settled"}),
        ));
        store
    }

    /// Auto-compaction (upstream `core/compaction/` loop): after a turn, if
    /// auto-compaction is enabled and the estimated context tokens exceed the
    /// model's window minus the reserve, summarize history through the facade
    /// and replace the in-memory context with the summary + retained tail.
    /// Returns true when compaction ran.
    async fn maybe_auto_compact(&mut self) -> Result<bool, String> {
        if !self.auto_compaction_enabled {
            return Ok(false);
        }
        let settings = pi_agent::harness::compaction::DEFAULT_COMPACTION_SETTINGS;
        let estimate = pi_agent::harness::compaction::estimate_context_tokens(&self.messages);
        if !pi_agent::harness::compaction::should_compact(
            estimate.tokens,
            self.model.context_window,
            &settings,
        ) {
            return Ok(false);
        }
        let entries = self.get_entries().await?;
        let Some(preparation) =
            pi_agent::harness::compaction::prepare_compaction(&entries, &settings)
                .map_err(|e| format!("auto-compact: prepare: {e}"))?
        else {
            return Ok(false);
        };
        let models = self.models.clone();
        let complete_simple_fn: pi_agent::harness::CompleteSimpleFn =
            Arc::new(move |model, ctx, opts| {
                let models = models.clone();
                let opts = opts.clone();
                let model = model.clone();
                let ctx = ctx.clone();
                Box::pin(async move { models.complete_simple(&model, &ctx, Some(&opts)).await })
            });
        let options = pi_agent::harness::SimpleModels { complete_simple_fn };
        let retry = pi_ai::utils::retry::RetryPolicy {
            enabled: false,
            max_retries: 0,
            base_delay_ms: 0,
        };
        let result = pi_agent::harness::compaction::compact(
            &preparation,
            &options,
            &self.model,
            None,
            None,
            None,
            Some(&retry),
            None,
        )
        .await
        .map_err(|e| format!("auto-compact: {e}"))?;

        let summary_msg = pi_agent::agent::user_text_prompt(
            format!("[Compaction summary]\n{}", result.summary),
            pi_ai::types::now_ms(),
        );
        let mut replaced = vec![summary_msg];
        replaced.extend(result.retained_tail.clone());
        self.messages = replaced;

        self.session
            .append_entry(
                EntryNoStats::Compaction {
                    id: format!("c-{}", pi_agent::session::new_id()),
                    summary: result.summary.clone(),
                    retained_tail: result.retained_tail,
                    tokens_before: result.tokens_before,
                    details: None,
                    usage: result.usage,
                },
                "main",
            )
            .await
            .map_err(|e| format!("auto-compact: persist: {e}"))?;
        Ok(true)
    }

    async fn persist_messages(
        &mut self,
        new_messages: &[pi_agent::types::AgentMessage],
    ) -> Result<(), String> {
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
                order: Some(EntryOrder::OldestFirst),
                id: None,
                entry_type: None,
                custom_type: None,
                cursor: None,
                limit: None,
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Rebuild the active branch context the same way the session layer does
    /// for a resumed session. `self.messages` is kept as the live agent state
    /// during a prompt, but it must be repopulated after switch/fork.
    async fn load_context_messages(&self) -> Result<Vec<pi_agent::types::AgentMessage>, String> {
        let entries = self
            .session
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                None,
                &BranchBounds::default(),
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(pi_agent::session::context::build_session_context(
            &entries,
            &pi_agent::session::context::SessionContextBuildOptions::default(),
        )
        .messages)
    }

    fn last_assistant_text(&self) -> Option<String> {
        self.messages.iter().rev().find_map(|message| {
            let pi_agent::types::AgentMessage::Core(Message::Assistant(assistant)) = message else {
                return None;
            };
            if assistant.stop_reason() == Some(pi_ai::types::StopReason::Aborted)
                && assistant.content().is_empty()
            {
                return None;
            }
            let text: String = assistant
                .content()
                .iter()
                .filter_map(|block| match block {
                    pi_ai::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        })
    }

    /// Build an RPC session-tree (upstream `SessionManager.getTree()`):
    /// parent-linked entries as `{ entry, children, label? }` nodes, roots
    /// first, each node's children sorted by entry timestamp ascending.
    /// Entries whose parent is absent, unresolved, or the entry itself are
    /// treated as roots (matches upstream, which also orphans on a missing or
    /// self-referential `parentId`). A `label` key is emitted only when the
    /// session has resolved a label for that entry id (upstream `labelsById`).
    fn build_tree(
        entries: &[pi_agent::session::types::Entry],
        labels: &HashMap<String, String>,
    ) -> serde_json::Value {
        let node_count = entries.len();
        let by_id: HashMap<String, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id().to_string(), i))
            .collect();

        // Node shells: `{ entry, children: [], label? }`.
        let shells: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                let mut node = serde_json::json!({ "entry": e, "children": [] });
                if let Some(label) = labels.get(e.id()) {
                    node["label"] = serde_json::Value::String(label.clone());
                }
                node
            })
            .collect();

        // Adjacency: which node indices are children of each parent.
        let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); node_count];
        let mut is_root = vec![true; node_count];
        for (i, entry) in entries.iter().enumerate() {
            if let Some(parent) = entry.parent_id() {
                if parent != entry.id() {
                    if let Some(&parent_idx) = by_id.get(parent) {
                        children_of[parent_idx].push(i);
                        is_root[i] = false;
                    }
                }
            }
        }

        // Sort each parent's children by entry timestamp ascending.
        for children in children_of.iter_mut() {
            children.sort_by_key(|&ci| entries[ci].timestamp());
        }

        // Build bottom-up (a child always follows its parent in the
        // oldest-first entries array, so reverse-index order builds children
        // before parents) — iterative, matching upstream's overflow-safe note.
        let mut result: Vec<serde_json::Value> = vec![serde_json::Value::Null; node_count];
        for i in (0..node_count).rev() {
            let mut node = shells[i].clone();
            let kids: Vec<serde_json::Value> = children_of[i]
                .iter()
                .map(|&ci| result[ci].clone())
                .collect();
            node["children"] = serde_json::Value::Array(kids);
            result[i] = node;
        }

        let roots: Vec<serde_json::Value> = (0..node_count)
            .filter(|&i| is_root[i])
            .map(|i| result[i].clone())
            .collect();
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
            pending_message_count: self.steering_queue.lock().unwrap().len()
                + self.follow_up_queue.lock().unwrap().len(),
        }
    }

    /// Execute a single parsed command; returns the JSON lines to write
    /// (responses + streamed events).
    pub async fn handle_command(
        &mut self,
        command: RpcCommand,
        store: &mut Vec<String>,
    ) -> Result<(), String> {
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
            "prompt" => {
                let Some(message) = command.str_field("message") else {
                    fail(store, &id, &cmd, "missing message".to_string());
                    return Ok(());
                };
                {
                    let mut lock = self.run_lock.lock().unwrap();
                    if *lock {
                        fail(
                            store,
                            &id,
                            &cmd,
                            "Agent is already streaming; send abort first".to_string(),
                        );
                        return Ok(());
                    }
                    *lock = true;
                }
                self.is_streaming = true;
                self.abort_signal.store(false, Ordering::SeqCst);
                self.abort_retry_signal.store(false, Ordering::SeqCst);
                // Preflight success response is emitted first (upstream
                // preflightResult).
                respond(store, success(id.as_deref(), &cmd, None));

                let RpcPromptRun {
                    prompts,
                    mut context,
                    config,
                } = self.prepare_prompt_run(&message);
                let mut captured_events = Vec::new();
                let persisted_messages = Arc::new(Mutex::new(Vec::new()));
                let persisted_for_loop = persisted_messages.clone();
                let new_messages =
                    run_rich_agent_loop(prompts, &mut context, &config, &mut |event| {
                        if let RichAgentEvent::MessageEnd { message } = &event {
                            persisted_for_loop.lock().unwrap().push(message.clone());
                        }
                        if let Some(line) = serialize_rpc_prompt_event(event) {
                            captured_events.push(line);
                        }
                    })
                    .await;

                // Emit the captured stream events in wire order.
                for line in captured_events {
                    store.push(line);
                }

                let persisted_messages = std::mem::take(&mut *persisted_messages.lock().unwrap());
                store.extend(
                    self.settle_prompt_with_persistence(new_messages, persisted_messages)
                        .await,
                );
                Ok(())
            }

            "steer" | "follow_up" => {
                let Some(message) = command.str_field("message") else {
                    fail(store, &id, &cmd, "missing message".to_string());
                    return Ok(());
                };
                let queued = pi_agent::agent::user_text_prompt(message, pi_ai::types::now_ms());
                if command.type_ == "steer" {
                    self.steering_queue.lock().unwrap().enqueue(queued);
                } else {
                    self.follow_up_queue.lock().unwrap().enqueue(queued);
                }
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "abort" => {
                self.abort_signal.store(true, Ordering::SeqCst);
                self.abort_bash.store(true, Ordering::SeqCst);
                self.abort_retry_signal.store(true, Ordering::SeqCst);
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
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({"cancelled": false})),
                    ),
                );
                Ok(())
            }

            // =================================================================
            // State
            // =================================================================
            "get_state" => {
                let state = self.build_session_state();
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::to_value(state).unwrap_or(serde_json::Value::Null)),
                    ),
                );
                Ok(())
            }

            // =================================================================
            // Model
            // =================================================================
            "set_model" => {
                let provider_name = command
                    .str_field("provider")
                    .ok_or_else(|| "missing provider".to_string())?;
                let model_id = command
                    .str_field("modelId")
                    .ok_or_else(|| "missing modelId".to_string())?;
                let model = self.models.get_model(&provider_name, &model_id);
                match model {
                    Some(model) => {
                        self.provider = provider_name.clone();
                        self.model = model.clone();
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(
                                    serde_json::to_value(model).unwrap_or(serde_json::Value::Null),
                                ),
                            ),
                        );
                        Ok(())
                    }
                    None => {
                        fail(
                            store,
                            &id,
                            &cmd,
                            format!("Model not found: {provider_name}/{model_id}"),
                        );
                        Ok(())
                    }
                }
            }

            "cycle_model" => {
                let available = self.available_models();
                let current = available
                    .iter()
                    .position(|m| m.provider == self.model.provider && m.id == self.model.id);
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
                        respond(
                            store,
                            success(id.as_deref(), &cmd, Some(serde_json::Value::Null)),
                        );
                        Ok(())
                    }
                }
            }

            "get_available_models" => {
                let models = self.available_models();
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({"models": models})),
                    ),
                );
                Ok(())
            }

            // =================================================================
            // Thinking
            // =================================================================
            "set_thinking_level" => {
                let level = command
                    .str_field("level")
                    .unwrap_or_else(|| "off".to_string());
                let parsed = level
                    .parse::<pi_ai::types::ModelThinkingLevel>()
                    .unwrap_or(pi_ai::types::ModelThinkingLevel::Off);
                self.thinking_level = parsed;
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "cycle_thinking_level" => {
                let available = pi_ai::model::get_supported_thinking_levels(&self.model);
                if available.is_empty() {
                    respond(
                        store,
                        success(id.as_deref(), &cmd, Some(serde_json::Value::Null)),
                    );
                    return Ok(());
                }
                let current = available.iter().position(|l| *l == self.thinking_level);
                let next_idx = match current {
                    Some(idx) => (idx + 1) % available.len(),
                    None => 0,
                };
                self.thinking_level = available[next_idx];
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "level": self.thinking_level.as_str() })),
                    ),
                );
                Ok(())
            }

            "get_available_thinking_levels" => {
                let levels = pi_ai::model::get_supported_thinking_levels(&self.model)
                    .into_iter()
                    .map(|l| l.as_str().to_string())
                    .collect::<Vec<_>>();
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "levels": levels })),
                    ),
                );
                Ok(())
            }

            // =================================================================
            // Queue modes
            // =================================================================
            "set_steering_mode" => {
                let mode = command
                    .str_field("mode")
                    .unwrap_or_else(|| "all".to_string());
                if !matches!(mode.as_str(), "all" | "one-at-a-time") {
                    fail(store, &id, &cmd, format!("Invalid steering mode: {mode}"));
                    return Ok(());
                }
                self.steering_mode = mode.clone();
                self.steering_queue.lock().unwrap().mode = if mode == "all" {
                    QueueMode::All
                } else {
                    QueueMode::OneAtATime
                };
                self.settings.set_steering_mode(&mode);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "set_follow_up_mode" => {
                let mode = command
                    .str_field("mode")
                    .unwrap_or_else(|| "all".to_string());
                if !matches!(mode.as_str(), "all" | "one-at-a-time") {
                    fail(store, &id, &cmd, format!("Invalid follow-up mode: {mode}"));
                    return Ok(());
                }
                self.follow_up_mode = mode.clone();
                self.follow_up_queue.lock().unwrap().mode = if mode == "all" {
                    QueueMode::All
                } else {
                    QueueMode::OneAtATime
                };
                self.settings.set_follow_up_mode(&mode);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            // =================================================================
            // Compaction / retry
            // =================================================================
            "set_auto_compaction" => {
                self.auto_compaction_enabled = command.bool_field("enabled").unwrap_or(true);
                self.settings
                    .set_compaction_enabled(self.auto_compaction_enabled);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "set_auto_retry" => {
                self.auto_retry_enabled = command.bool_field("enabled").unwrap_or(true);
                self.settings.set_retry_enabled(self.auto_retry_enabled);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "abort_retry" => {
                self.abort_retry_signal.store(true, Ordering::SeqCst);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "compact" => {
                // Run the ported harness compaction over the current session
                // entries, summarizing through the facade's complete_simple.
                // Errors are reported as failure responses (never terminate
                // the RPC loop).
                let entries = match self.get_entries().await {
                    Ok(e) => e,
                    Err(e) => {
                        fail(store, &id, &cmd, e.clone());
                        return Ok(());
                    }
                };
                let prepared = match pi_agent::harness::compaction::prepare_compaction(
                    &entries,
                    &pi_agent::harness::compaction::DEFAULT_COMPACTION_SETTINGS,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        let msg = format!("prepare compaction: {e}");
                        fail(store, &id, &cmd, msg);
                        return Ok(());
                    }
                };
                let result = match prepared {
                    None => {
                        // Nothing to compact.
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(serde_json::json!({
                                    "summary": "",
                                    "tokensBefore": 0,
                                })),
                            ),
                        );
                        return Ok(());
                    }
                    Some(preparation) => {
                        let models = self.models.clone();
                        let complete_simple_fn: CompleteSimpleFn = Arc::new(
                            move |model: &Model,
                                  ctx: &pi_ai::types::Context,
                                  opts: &SimpleStreamOptions| {
                                let models = models.clone();
                                let opts = opts.clone();
                                let model = model.clone();
                                let ctx = ctx.clone();
                                Box::pin(async move {
                                    models.complete_simple(&model, &ctx, Some(&opts)).await
                                })
                                    as BoxFuture<'static, AssistantMessage>
                            },
                        );
                        let options = SimpleModels { complete_simple_fn };
                        let model = self.model.clone();
                        let retry = pi_ai::utils::retry::RetryPolicy {
                            enabled: false,
                            max_retries: 0,
                            base_delay_ms: 0,
                        };
                        let result = match pi_agent::harness::compaction::compact(
                            &preparation,
                            &options,
                            &model,
                            command.str_field("customInstructions").as_deref(),
                            None,
                            None,
                            Some(&retry),
                            None,
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                let msg = format!("compact: {e}");
                                fail(store, &id, &cmd, msg);
                                return Ok(());
                            }
                        };
                        serde_json::json!({
                            "message": null,
                            "summary": result.summary,
                            "tokensBefore": result.tokens_before,
                        })
                    }
                };
                respond(store, success(id.as_deref(), &cmd, Some(result)));
                Ok(())
            }

            // =================================================================
            // Bash
            // =================================================================
            "bash" => {
                let bash_command = command
                    .str_field("command")
                    .ok_or_else(|| "missing command".to_string())?;
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
                let user_messages = stats
                    .iter()
                    .filter(|e| {
                        e.as_message().is_some_and(|m| {
                            matches!(m, pi_agent::types::AgentMessage::Core(Message::User(_)))
                        })
                    })
                    .count();
                let assistant_messages = stats
                    .iter()
                    .filter(|e| {
                        e.as_message().is_some_and(|m| {
                            matches!(
                                m,
                                pi_agent::types::AgentMessage::Core(Message::Assistant(_))
                            )
                        })
                    })
                    .count();
                let tool_calls = stats.iter().filter(|e| e.as_message().is_some_and(|m| matches!(m, pi_agent::types::AgentMessage::Core(Message::Assistant(a)) if a.content().iter().any(|b| matches!(b, pi_ai::types::ContentBlock::ToolCall { .. }))))).count();
                let tool_results = stats
                    .iter()
                    .filter(|e| {
                        e.as_message().is_some_and(|m| {
                            matches!(
                                m,
                                pi_agent::types::AgentMessage::Core(Message::ToolResult(_))
                            )
                        })
                    })
                    .count();
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({
                            "sessionFile": self.session_path,
                            "sessionId": self.session_id,
                            "userMessages": user_messages,
                            "assistantMessages": assistant_messages,
                            "toolCalls": tool_calls,
                            "toolResults": tool_results,
                            "totalMessages": user_messages + assistant_messages + tool_calls + tool_results,
                            "tokens": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
                            "cost": 0,
                        })),
                    ),
                );
                Ok(())
            }

            "export_html" => {
                let Some(session_path) = self.session_path.clone() else {
                    fail(
                        store,
                        &id,
                        &cmd,
                        "Cannot export in-memory session to HTML".to_string(),
                    );
                    return Ok(());
                };
                let output_path = command.str_field("outputPath").map(|s| s.to_string());
                match crate::core::export_html::export_session_file(
                    &session_path,
                    output_path.as_deref(),
                    None,
                ) {
                    Ok(path) => {
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(serde_json::json!({ "path": path })),
                            ),
                        );
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e.to_string());
                        Ok(())
                    }
                }
            }

            "switch_session" => {
                let session_path = command
                    .str_field("sessionPath")
                    .ok_or_else(|| "missing sessionPath".to_string())?;
                match self.load_session(&session_path).await {
                    Ok(()) => {
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(serde_json::json!({"cancelled": false})),
                            ),
                        );
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e);
                        Ok(())
                    }
                }
            }

            "fork" => {
                let entry_id = command
                    .str_field("entryId")
                    .ok_or_else(|| "missing entryId".to_string())?;
                match self.fork_session(Some(entry_id)).await {
                    Ok(_) => {
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(serde_json::json!({"text": "", "cancelled": false})),
                            ),
                        );
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e);
                        Ok(())
                    }
                }
            }

            "clone" => match self.fork_session(None).await {
                Ok(_) => {
                    respond(
                        store,
                        success(
                            id.as_deref(),
                            &cmd,
                            Some(serde_json::json!({"cancelled": false})),
                        ),
                    );
                    Ok(())
                }
                Err(e) => {
                    fail(store, &id, &cmd, e);
                    Ok(())
                }
            },

            "get_fork_messages" => {
                let entries = self.get_entries().await.map_err(|e| {
                    fail(store, &id, &cmd, e.clone());
                    e
                })?;
                let messages: Vec<serde_json::Value> = entries
                    .iter()
                    .filter_map(|entry| {
                        let message = entry.as_message()?;
                        let pi_agent::types::AgentMessage::Core(Message::User(user)) = message
                        else {
                            return None;
                        };
                        let text = pi_agent::agent::user_content_text(user);
                        (!text.is_empty())
                            .then(|| serde_json::json!({ "entryId": entry.id(), "text": text }))
                    })
                    .collect();
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "messages": messages })),
                    ),
                );
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
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "entries": entries, "leafId": leaf_id })),
                    ),
                );
                Ok(())
            }

            "get_tree" => {
                let entries = self.get_entries().await.map_err(|e| {
                    fail(store, &id, &cmd, e.clone());
                    e
                })?;
                let mut labels: HashMap<String, String> = HashMap::new();
                for entry in &entries {
                    let entry_id = entry.id().to_string();
                    if let Some(label) = self.session.get_label(entry.id()).await {
                        labels.insert(entry_id, label);
                    }
                }
                let tree = Self::build_tree(&entries, &labels);
                let leaf_id = self.session.get_leaf_id().await.ok().flatten();
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "tree": tree, "leafId": leaf_id })),
                    ),
                );
                Ok(())
            }

            "get_last_assistant_text" => {
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "text": self.last_assistant_text() })),
                    ),
                );
                Ok(())
            }

            "set_session_name" => {
                let name = command
                    .str_field("name")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
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
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "messages": self.messages })),
                    ),
                );
                Ok(())
            }

            "get_commands" => {
                respond(
                    store,
                    success(
                        id.as_deref(),
                        &cmd,
                        Some(serde_json::json!({ "commands": [] })),
                    ),
                );
                Ok(())
            }

            other => {
                fail(store, &id, other, format!("Unknown command: {other}"));
                Ok(())
            }
        }
    }

    async fn load_session(&mut self, path: &str) -> Result<(), String> {
        // Load directly from the supplied path rather than looking it up in
        // the current session root: RPC clients may switch to a session from
        // another cwd/session directory.
        let storage = pi_agent::session::JsonlSessionStorage::load(
            pi_agent::fs::StdFileSystem::new(&self.cwd),
            path,
        )
        .await
        .map_err(|e| format!("failed to open session {path:?}: {e}"))?;
        let session = JsonlSession::new(storage);
        let meta = session.get_metadata().await;
        self.session = session;
        self.session_path = Some(meta.path);
        self.session_id = meta.id;
        self.session_name = self.session.get_name().await;
        self.messages = self.load_context_messages().await?;
        Ok(())
    }

    async fn fork_session(&mut self, entry_id: Option<String>) -> Result<(), String> {
        let metadata = SessionMetadata {
            id: self.session_id.clone(),
            created_at: 0,
            cwd: self.cwd.clone(),
            path: self.session_path.clone().unwrap_or_else(|| "".to_string()),
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
        self.session_id = meta.id;
        self.session_name = self.session.get_name().await;
        self.messages = self.load_context_messages().await?;
        Ok(())
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
    let usage = event
        .partial()
        .and_then(|p| p.usage())
        .or_else(|| match event {
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error {
                error_message: message,
                ..
            } => message.usage(),
            _ => None,
        })
        .cloned();
    let (kind, mut body) = event_json(event);
    let usage = usage
        .map(|u| serde_json::to_value(u))
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(serde_json::Value::Null);
    body.insert("type".to_string(), serde_json::json!("message_update"));
    body.insert("usage".to_string(), usage);
    body.insert(
        "assistantMessageEvent".to_string(),
        serde_json::Value::Object(kind),
    );
    serde_json::Value::Object(body)
}

fn event_json(
    event: &AssistantMessageEvent,
) -> (
    serde_json::Map<String, serde_json::Value>,
    serde_json::Map<String, serde_json::Value>,
) {
    match event {
        AssistantMessageEvent::Start { partial } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("message_start"));
            m.insert(
                "message".into(),
                serde_json::to_value(partial).unwrap_or(serde_json::Value::Null),
            );
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::TextStart { content_index, .. } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("text_start"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("text_delta"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("delta".into(), serde_json::json!(delta));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::TextEnd {
            content_index,
            content,
            ..
        } => {
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
        AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            ..
        } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("thinking_delta"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("delta".into(), serde_json::json!(delta));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ThinkingEnd {
            content_index,
            content,
            ..
        } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("thinking_end"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("content".into(), serde_json::json!(content));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ToolCallStart {
            content_index,
            partial,
            ..
        } => {
            let (id, tool_name) = partial
                .content()
                .get(*content_index)
                .map(|b| match b {
                    pi_ai::types::ContentBlock::ToolCall { id, name, .. } => {
                        (id.clone(), Some(name.clone()))
                    }
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
        AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("toolcall_delta"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert("delta".into(), serde_json::json!(delta));
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("toolcall_end"));
            m.insert("contentIndex".into(), serde_json::json!(content_index));
            m.insert(
                "toolCall".into(),
                serde_json::to_value(tool_call).unwrap_or(serde_json::Value::Null),
            );
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
            m.insert(
                "message".into(),
                serde_json::to_value(message).unwrap_or(serde_json::Value::Null),
            );
            (m, serde_json::Map::new())
        }
        AssistantMessageEvent::Error {
            reason,
            error_message,
        } => {
            let mut m = serde_json::Map::new();
            m.insert("type".into(), serde_json::json!("error"));
            let reason_str = match reason {
                pi_ai::types::ErrorReason::Aborted => "aborted",
                pi_ai::types::ErrorReason::Error => "error",
            };
            m.insert("reason".into(), serde_json::json!(reason_str));
            m.insert(
                "error".into(),
                serde_json::to_value(error_message).unwrap_or(serde_json::Value::Null),
            );
            (m, serde_json::Map::new())
        }
    }
}

fn parse_rpc_input(line: &str) -> Result<RpcCommand, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("Failed to parse command: {e}"))?;
    RpcCommand::parse(parsed)
}

fn can_handle_during_prompt(command: &RpcCommand) -> bool {
    matches!(
        command.type_.as_str(),
        "abort"
            | "abort_retry"
            | "steer"
            | "follow_up"
            | "get_state"
            | "set_steering_mode"
            | "set_follow_up_mode"
            | "prompt"
    )
}

async fn handle_rpc_prompt_task_message<W: AsyncWrite + Unpin>(
    runtime: &mut RpcRuntime,
    message: Option<RpcPromptTaskMessage>,
    out: &mut W,
    pending_abort_responses: &mut VecDeque<String>,
) -> Result<bool, String> {
    match message {
        Some(RpcPromptTaskMessage::Event(line)) => {
            write_rpc_lines(out, std::iter::once(line)).await?;
            Ok(false)
        }
        Some(RpcPromptTaskMessage::Finished(result)) => {
            let store = runtime
                .settle_prompt_with_persistence(result.new_messages, result.persisted_messages)
                .await;
            write_rpc_lines(out, store).await?;
            let abort_responses: Vec<String> = pending_abort_responses.drain(..).collect();
            write_rpc_lines(out, abort_responses).await?;
            Ok(true)
        }
        None => Err("RPC prompt task ended without a completion message".to_string()),
    }
}

/// Run the RPC mode loop: read commands from stdin, write responses/events
/// to stdout until EOF.
pub async fn run_rpc_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut runtime = RpcRuntime::new(args, settings).await?;
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = JsonlLineReader::new(stdin);
    let mut out = tokio::io::BufWriter::new(stdout);
    let mut active_prompt: Option<UnboundedReceiver<RpcPromptTaskMessage>> = None;
    let mut pending_commands = VecDeque::new();
    let mut pending_abort_responses = VecDeque::new();
    let mut input_closed = false;

    loop {
        if active_prompt.is_some() {
            if input_closed {
                let message = active_prompt
                    .as_mut()
                    .expect("active prompt receiver")
                    .recv()
                    .await;
                if handle_rpc_prompt_task_message(
                    &mut runtime,
                    message,
                    &mut out,
                    &mut pending_abort_responses,
                )
                .await?
                {
                    active_prompt = None;
                }
                continue;
            }

            // Keep stdin intake live while the detached prompt sends stream
            // events. Tokio's fair selection prevents a high-volume event
            // stream from starving control commands; the worker channel still
            // preserves event order internally.
            let outcome = {
                let receiver = active_prompt.as_mut().expect("active prompt receiver");
                tokio::select! {
                    message = receiver.recv() => Ok(message),
                    line = reader.next_line() => Err(line),
                }
            };
            match outcome {
                Ok(message) => {
                    if handle_rpc_prompt_task_message(
                        &mut runtime,
                        message,
                        &mut out,
                        &mut pending_abort_responses,
                    )
                    .await?
                    {
                        active_prompt = None;
                    }
                }
                Err(line_result) => {
                    let Some(line) = line_result.map_err(|e| format!("stdin read error: {e}"))?
                    else {
                        input_closed = true;
                        continue;
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let command = match parse_rpc_input(&line) {
                        Ok(command) => command,
                        Err(error) => {
                            let response = failure(None, "parse", error);
                            write_rpc_lines(
                                &mut out,
                                std::iter::once(serialize_json_line(&response)),
                            )
                            .await?;
                            continue;
                        }
                    };
                    if can_handle_during_prompt(&command) {
                        let is_abort = command.type_ == "abort";
                        let mut store = Vec::new();
                        runtime.handle_command(command, &mut store).await?;
                        if is_abort {
                            pending_abort_responses.extend(store);
                        } else {
                            write_rpc_lines(&mut out, store).await?;
                        }
                    } else {
                        pending_commands.push_back(command);
                    }
                }
            }
            continue;
        }

        if let Some(command) = pending_commands.pop_front() {
            if command.type_ == "prompt" {
                let mut store = Vec::new();
                if let Some(receiver) = runtime.start_prompt_task(command, &mut store) {
                    write_rpc_lines(&mut out, store).await?;
                    active_prompt = Some(receiver);
                } else {
                    write_rpc_lines(&mut out, store).await?;
                }
            } else {
                let mut store = Vec::new();
                runtime.handle_command(command, &mut store).await?;
                write_rpc_lines(&mut out, store).await?;
            }
            continue;
        }

        if input_closed {
            break;
        }

        let Some(line) = reader
            .next_line()
            .await
            .map_err(|e| format!("stdin read error: {e}"))?
        else {
            input_closed = true;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let command = match parse_rpc_input(&line) {
            Ok(command) => command,
            Err(error) => {
                let response = failure(None, "parse", error);
                write_rpc_lines(&mut out, std::iter::once(serialize_json_line(&response))).await?;
                continue;
            }
        };
        if command.type_ == "prompt" {
            let mut store = Vec::new();
            if let Some(receiver) = runtime.start_prompt_task(command, &mut store) {
                write_rpc_lines(&mut out, store).await?;
                active_prompt = Some(receiver);
            } else {
                write_rpc_lines(&mut out, store).await?;
            }
        } else {
            let mut store = Vec::new();
            runtime.handle_command(command, &mut store).await?;
            write_rpc_lines(&mut out, store).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn runtime_for_test() -> RpcRuntime {
        // Fully hermetic: pin an explicit faux model and a fresh session id/dir
        // so host-shell env (PI_MODEL / PI_SESSION_ID / PI_PROVIDER) cannot
        // leak into the runtime construction.
        let root = std::env::temp_dir().join(format!("pi-rpc-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let args = crate::args::parse_args(&[
            "--provider".to_string(),
            "faux".to_string(),
            "--model".to_string(),
            "faux-1".to_string(),
            "--session-id".to_string(),
            pi_agent::session::new_id(),
            "--session-dir".to_string(),
            root.join("sessions").to_string_lossy().into_owned(),
            "--no-tools".to_string(),
        ])
        .expect_run();
        let settings = SettingsManager::in_memory(Default::default());
        RpcRuntime::new(&args, settings).await.unwrap()
    }

    async fn run_test_prompt(runtime: &mut RpcRuntime, message: &str) {
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": message}))
                    .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
    }

    fn assistant_count(runtime: &RpcRuntime) -> usize {
        runtime
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    pi_agent::types::AgentMessage::Core(Message::Assistant(_))
                )
            })
            .count()
    }

    #[test]
    fn to_json_event_strips_partial() {
        let mut partial = pi_ai::types::AssistantMessage::new();
        partial.set_api_provider_model("faux", "faux", "faux-1");
        partial.set_usage(pi_ai::types::Usage::default());
        let event = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hi".into(),
            partial: partial.clone(),
        };
        let json = to_json_message_update(&event);
        assert_eq!(json["type"], "message_update");
        assert_eq!(json["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(json["assistantMessageEvent"]["delta"], "hi");
        assert!(json["assistantMessageEvent"].get("partial").is_none());
    }

    #[test]
    fn retry_terminal_event_preserves_usage_and_stop_reason() {
        let mut message = pi_ai::types::AssistantMessage::new();
        message.set_api_provider_model("faux", "faux", "faux-1");
        message.set_usage(pi_ai::types::Usage::default());
        message.set_stop_reason(pi_ai::types::StopReason::Error);
        let json = serialize_rpc_prompt_event(RichAgentEvent::MessageEnd {
            message: pi_agent::types::AgentMessage::Core(Message::Assistant(message)),
        })
        .expect("assistant terminal event should serialize");
        let json: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
        assert_eq!(json["assistantMessageEvent"]["type"], "error");
        assert_eq!(json["assistantMessageEvent"]["reason"], "error");
        assert_eq!(json["usage"]["input"], 0);
        assert_eq!(
            json["assistantMessageEvent"]["error"]["stopReason"],
            "error"
        );
    }

    #[test]
    fn retry_status_events_use_upstream_wire_names() {
        let start = serialize_rpc_prompt_event(RichAgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 250,
            error_message: "overloaded_error".to_string(),
        })
        .unwrap();
        let start: serde_json::Value = serde_json::from_str(start.trim()).unwrap();
        assert_eq!(start["type"], "auto_retry_start");
        assert_eq!(start["maxAttempts"], 3);
        assert_eq!(start["delayMs"], 250);
        assert_eq!(start["errorMessage"], "overloaded_error");

        let end = serialize_rpc_prompt_event(RichAgentEvent::AutoRetryEnd {
            success: false,
            attempt: 1,
            final_error: Some("Retry cancelled".to_string()),
        })
        .unwrap();
        let end: serde_json::Value = serde_json::from_str(end.trim()).unwrap();
        assert_eq!(end["type"], "auto_retry_end");
        assert_eq!(end["success"], false);
        assert_eq!(end["finalError"], "Retry cancelled");
    }

    #[tokio::test]
    async fn settle_retry_persists_failed_attempts_but_keeps_live_context_clean() {
        let mut runtime = runtime_for_test().await;
        let prompt = pi_agent::agent::user_text_prompt("retry", 1);
        let mut failed = pi_ai::types::AssistantMessage::new();
        failed.set_stop_reason(pi_ai::types::StopReason::Error);
        let mut recovered = pi_ai::types::AssistantMessage::new();
        recovered.set_stop_reason(pi_ai::types::StopReason::Stop);
        let failed = pi_agent::types::AgentMessage::Core(Message::Assistant(failed));
        let recovered = pi_agent::types::AgentMessage::Core(Message::Assistant(recovered));

        runtime
            .settle_prompt_with_persistence(
                vec![prompt.clone(), recovered.clone()],
                vec![prompt, failed, recovered],
            )
            .await;

        assert_eq!(runtime.messages.len(), 2);
        let entries = runtime.get_entries().await.unwrap();
        let messages: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry.as_message())
            .collect();
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[1],
            pi_agent::types::AgentMessage::Core(Message::Assistant(ref message))
                if message.stop_reason() == Some(pi_ai::types::StopReason::Error)
        ));
        assert!(matches!(
            messages[2],
            pi_agent::types::AgentMessage::Core(Message::Assistant(ref message))
                if message.stop_reason() == Some(pi_ai::types::StopReason::Stop)
        ));
    }

    #[tokio::test]
    async fn detached_retry_streams_failed_deltas_and_status_before_settling() {
        let core = pi_ai::providers::FauxProviderCore::new(
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        core.set_responses(vec![
            pi_ai::providers::FauxResponseStep::Message(pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text("partial failure")],
                pi_ai::providers::FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::Error),
                    error_message: Some("overloaded_error".to_string()),
                    ..Default::default()
                },
            )),
            pi_ai::providers::FauxResponseStep::Message(pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text("recovered")],
                pi_ai::providers::FauxAssistantOptions::default(),
            )),
        ]);
        let model = core.get_model(None).unwrap().clone();
        let stream_core = core.clone();
        let stream_fn: pi_agent::agent::StreamFn =
            Arc::new(move |model, context| stream_core.stream(model, context, None));
        let context = AgentContext::new(Some("test".to_string()), Vec::new());
        let mut config = RichAgentLoopConfig::new(model.clone(), stream_fn, None);
        config.retry_policy = Some(pi_ai::utils::retry::RetryPolicy {
            enabled: true,
            max_retries: 1,
            base_delay_ms: 0,
        });
        config.retry_signal = Some(Arc::new(AtomicBool::new(false)));
        let run = RpcPromptRun {
            prompts: vec![pi_agent::agent::user_text_prompt("retry", 1)],
            context,
            config,
        };
        let (sender, mut receiver) = mpsc::unbounded_channel();
        run_rpc_prompt(run, sender).await;

        let mut lines = Vec::new();
        let mut result = None;
        while let Some(message) = receiver.recv().await {
            match message {
                RpcPromptTaskMessage::Event(line) => lines.push(line),
                RpcPromptTaskMessage::Finished(value) => {
                    result = Some(value);
                    break;
                }
            }
        }
        let result = result.expect("detached retry should settle");
        assert_eq!(result.new_messages.len(), 2);
        assert_eq!(result.persisted_messages.len(), 3);
        let values: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line.trim()).unwrap())
            .collect();
        let retry_start = values
            .iter()
            .position(|value| value["type"] == "auto_retry_start")
            .expect("retry start event");
        let retry_end = values
            .iter()
            .position(|value| value["type"] == "auto_retry_end")
            .expect("retry end event");
        assert!(retry_start < retry_end);
        assert!(values[..retry_start]
            .iter()
            .any(|value| value["assistantMessageEvent"]["type"] == "error"));
        assert!(values[..retry_start]
            .iter()
            .any(|value| value["assistantMessageEvent"]["type"] == "text_delta"));
        assert!(values[retry_start..retry_end]
            .iter()
            .any(|value| value["assistantMessageEvent"]["type"] == "done"));
    }

    #[tokio::test]
    async fn get_state_returns_shape() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        let line = store[0].trim();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "response");
        assert_eq!(v["command"], "get_state");
        assert_eq!(v["success"], true);
        assert!(
            v["data"]["sessionId"].is_string(),
            "data was: {}",
            v["data"]
        );
    }

    #[tokio::test]
    async fn queue_commands_update_pending_state_and_modes() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "type": "steer",
                    "message": "interrupt"
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "type": "follow_up",
                    "message": "continue"
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "type": "set_steering_mode",
                    "mode": "one-at-a-time"
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let mut state = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({ "type": "get_state" })).unwrap(),
                &mut state,
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(state[0].trim()).unwrap();
        assert_eq!(value["data"]["steeringMode"], "one-at-a-time");
        assert_eq!(value["data"]["pendingMessageCount"], 2);

        let mut invalid = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "type": "set_follow_up_mode",
                    "mode": "invalid"
                }))
                .unwrap(),
                &mut invalid,
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(invalid[0].trim()).unwrap();
        assert_eq!(value["success"], false);
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
            .handle_command(
                RpcCommand::parse(
                    serde_json::json!({"type": "set_thinking_level", "level": "high"}),
                )
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(),
                &mut store,
            )
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
            .handle_command(
                RpcCommand::parse(
                    serde_json::json!({"type": "set_session_name", "name": "my session"}),
                )
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], true);
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(),
                &mut store,
            )
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
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hello"}))
                    .unwrap(),
                &mut store,
            )
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
    async fn detached_prompt_accepts_control_commands_before_settling() {
        let mut runtime = runtime_for_test().await;
        let mut preflight = Vec::new();
        let receiver = runtime
            .start_prompt_task(
                RpcCommand::parse(
                    serde_json::json!({"id": "prompt-1", "type": "prompt", "message": "hello"}),
                )
                .unwrap(),
                &mut preflight,
            )
            .expect("prompt should start");
        let preflight_value: serde_json::Value = serde_json::from_str(preflight[0].trim()).unwrap();
        assert_eq!(preflight_value["id"], "prompt-1");
        assert_eq!(preflight_value["success"], true);

        // These commands run on the input-loop side while the worker owns
        // only its detached context/configuration.
        let mut controls = Vec::new();
        for command in [
            serde_json::json!({"id": "steer-mode", "type": "set_steering_mode", "mode": "all"}),
            serde_json::json!({"id": "follow-mode", "type": "set_follow_up_mode", "mode": "all"}),
            serde_json::json!({"id": "steer-1", "type": "steer", "message": "interrupt"}),
            serde_json::json!({"id": "follow-1", "type": "follow_up", "message": "continue"}),
            serde_json::json!({"id": "abort-1", "type": "abort"}),
        ] {
            runtime
                .handle_command(RpcCommand::parse(command).unwrap(), &mut controls)
                .await
                .unwrap();
        }
        assert_eq!(controls.len(), 5);
        assert!(controls.iter().all(|line| {
            serde_json::from_str::<serde_json::Value>(line.trim())
                .map(|value| value["success"] == true)
                .unwrap_or(false)
        }));
        assert!(runtime.abort_signal.load(Ordering::SeqCst));

        let mut state = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "state-1",
                    "type": "get_state"
                }))
                .unwrap(),
                &mut state,
            )
            .await
            .unwrap();
        let state_value: serde_json::Value = serde_json::from_str(state[0].trim()).unwrap();
        assert_eq!(state_value["data"]["isStreaming"], true);
        assert_eq!(state_value["data"]["pendingMessageCount"], 2);

        let mut receiver = receiver;
        let mut new_messages = None;
        while let Some(message) = receiver.recv().await {
            if let RpcPromptTaskMessage::Finished(result) = message {
                new_messages = Some(result.new_messages);
                break;
            }
        }
        let new_messages = new_messages.expect("prompt should finish");
        assert!(new_messages.iter().any(|message| {
            matches!(
                message,
                pi_agent::types::AgentMessage::Core(Message::Assistant(assistant))
                    if assistant.stop_reason() == Some(pi_ai::types::StopReason::Aborted)
            )
        }));
        runtime
            .settle_prompt_with_persistence(new_messages.clone(), new_messages)
            .await;
        assert!(!runtime.is_streaming);
        assert!(!*runtime.run_lock.lock().unwrap());
    }

    #[tokio::test]
    async fn queue_modes_control_steering_and_follow_up_drain_batches() {
        for (mode, expected_steering_assistants) in [("all", 1), ("one-at-a-time", 2)] {
            let mut runtime = runtime_for_test().await;
            let mut store = Vec::new();
            runtime
                .handle_command(
                    RpcCommand::parse(serde_json::json!({
                        "type": "set_steering_mode",
                        "mode": mode
                    }))
                    .unwrap(),
                    &mut store,
                )
                .await
                .unwrap();
            for message in ["steer-a", "steer-b"] {
                runtime
                    .handle_command(
                        RpcCommand::parse(serde_json::json!({"type": "steer", "message": message}))
                            .unwrap(),
                        &mut store,
                    )
                    .await
                    .unwrap();
            }
            run_test_prompt(&mut runtime, "prompt").await;
            assert_eq!(assistant_count(&runtime), expected_steering_assistants);
        }

        for (mode, expected_follow_up_assistants) in [("all", 2), ("one-at-a-time", 3)] {
            let mut runtime = runtime_for_test().await;
            let mut store = Vec::new();
            runtime
                .handle_command(
                    RpcCommand::parse(serde_json::json!({
                        "type": "set_follow_up_mode",
                        "mode": mode
                    }))
                    .unwrap(),
                    &mut store,
                )
                .await
                .unwrap();
            for message in ["follow-a", "follow-b"] {
                runtime
                    .handle_command(
                        RpcCommand::parse(serde_json::json!({
                            "type": "follow_up",
                            "message": message
                        }))
                        .unwrap(),
                        &mut store,
                    )
                    .await
                    .unwrap();
            }
            run_test_prompt(&mut runtime, "prompt").await;
            assert_eq!(assistant_count(&runtime), expected_follow_up_assistants);
        }
    }

    #[tokio::test]
    async fn steering_drains_before_follow_up_at_turn_boundaries() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        for (kind, message) in [("steer", "steering"), ("follow_up", "follow-up")] {
            runtime
                .handle_command(
                    RpcCommand::parse(serde_json::json!({"type": kind, "message": message}))
                        .unwrap(),
                    &mut store,
                )
                .await
                .unwrap();
        }
        run_test_prompt(&mut runtime, "prompt").await;

        let messages: Vec<serde_json::Value> = runtime
            .messages
            .iter()
            .map(|message| serde_json::to_value(message).unwrap())
            .collect();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["content"][0]["text"], "prompt");
        assert_eq!(messages[1]["content"][0]["text"], "steering");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["content"][0]["text"], "follow-up");
        assert_eq!(messages[4]["role"], "assistant");
    }

    #[tokio::test]
    async fn get_messages_after_prompt() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hi"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_messages"})).unwrap(),
                &mut store,
            )
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
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hello"}))
                    .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_entries"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        let entries = v["data"]["entries"].as_array().unwrap();
        assert!(entries.len() >= 2);
        assert_eq!(entries[0]["type"], "message");
    }

    #[tokio::test]
    async fn session_queries_use_reloaded_branch_context() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "hello"}))
                    .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let session_path = runtime.session_path.clone().expect("session path exists");
        let session_id = runtime.session_id.clone();

        runtime.load_session(&session_path).await.unwrap();
        assert_eq!(runtime.session_id, session_id);

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_last_assistant_text"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert!(response["data"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("faux response")));

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_messages"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(response["data"]["messages"].as_array().unwrap().len(), 2);

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_entries"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        let entries = response["data"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(response["data"]["leafId"].is_string());
        let first_entry_id = entries[0]["id"].as_str().unwrap();

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(
                    serde_json::json!({"type": "get_entries", "since": first_entry_id}),
                )
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(response["data"]["entries"].as_array().unwrap().len(), 1);

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_tree"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(response["data"]["tree"].as_array().unwrap().len(), 1);
        assert!(response["data"]["leafId"].is_string());

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_fork_messages"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        let fork_messages = response["data"]["messages"].as_array().unwrap();
        assert_eq!(fork_messages.len(), 1);
        assert_ne!(fork_messages[0]["entryId"], "-");
        assert_eq!(fork_messages[0]["text"], "hello");
    }

    #[tokio::test]
    async fn compact_empty_session_returns_zero_result() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "compact"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["tokensBefore"], 0);
    }

    #[tokio::test]
    async fn unknown_command_errors() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "nope"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("Unknown command: nope"));
    }

    #[tokio::test]
    async fn faux_provider_is_registered_in_facade_for_compact() {
        // Regression for the known divergence: RPC compact needs a
        // facade-registered provider; faux is intentionally absent from the
        // builtin registry, so the runtime must register it.
        let runtime = runtime_for_test().await;
        let provider = runtime
            .models
            .get_provider("faux")
            .expect("faux registered");
        assert!(
            provider.single_streams.is_some(),
            "faux has a stream implementation"
        );
        assert_eq!(
            runtime.models.get_models(None).len(),
            runtime.models.get_models(None).len()
        );
        // complete_simple must resolve through the facade rather than erroring
        // with "no API implementation".
        let model = runtime.model.clone();
        let ctx = pi_ai::types::Context {
            system_prompt: Some("summarize".to_string()),
            messages: vec![pi_ai::types::Message::User(
                pi_ai::types::UserContent::blocks(
                    vec![pi_ai::types::ContentBlock::text("hello")],
                    1,
                ),
            )],
            tools: vec![],
        };
        let options = pi_ai::types::SimpleStreamOptions::default();
        let msg = runtime
            .models
            .complete_simple(&model, &ctx, Some(&options))
            .await;
        assert!(
            msg.error_message().is_none(),
            "faux complete_simple should not error: {:?}",
            msg.error_message()
        );
    }

    #[tokio::test]
    async fn export_html_writes_file() {
        let mut runtime = runtime_for_test().await;
        let session_path = runtime.session_path.clone().expect("session path exists");
        let out_path = std::env::temp_dir().join(format!(
            "pi-rpc-export-{}-{}.html",
            std::process::id(),
            line!()
        ));
        let out = out_path.to_string_lossy().into_owned();
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "export_html", "outputPath": out}))
                    .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["type"], "response");
        assert_eq!(v["command"], "export_html");
        assert_eq!(v["success"], true, "export_html failed: {:?}", v);
        assert_eq!(v["data"]["path"], out);
        assert!(
            std::path::Path::new(&out).exists(),
            "export file was not written"
        );
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("Session Export"));
        assert!(html.contains("message"));
        std::fs::remove_file(&out).ok();
        let _ = session_path;
    }

    #[tokio::test]
    async fn export_html_without_session_errors() {
        let mut runtime = runtime_for_test().await;
        runtime.session_path = None;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "export_html"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["error"], "Cannot export in-memory session to HTML");
    }

    #[tokio::test]
    async fn second_turn_sees_first_turn_history() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "first"}))
                    .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let first_text = last_assistant_text(&store);
        assert!(
            first_text.contains("context messages: 1"),
            "first turn should see 1 context message (its own prompt): {first_text}"
        );

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": "second"}))
                    .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let second_text = last_assistant_text(&store);
        // Turn 2 context = [user(first), assistant(first), user(second)] = 3.
        assert!(
            second_text.contains("context messages: 3"),
            "second turn should see accumulated history: {second_text}"
        );
    }

    fn last_assistant_text(store: &[String]) -> String {
        for line in store.iter().rev() {
            let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            if v["type"] == "message_update" {
                // The done event carries the full assistant message.
                let event = &v["assistantMessageEvent"];
                if event["type"] == "done" {
                    if let Some(text) = event["message"]["content"].as_array() {
                        let joined: String = text
                            .iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect();
                        if !joined.is_empty() {
                            return joined;
                        }
                    }
                }
            }
        }
        String::new()
    }

    #[tokio::test]
    async fn auto_compaction_triggers_and_persists_entry() {
        let mut runtime = runtime_for_test().await;
        // Tiny window so the threshold triggers; register faux in the facade
        // (runtime_for_test already does) and set the env key for auth.
        std::env::set_var("FAUX_API_KEY", "test");
        runtime.model.context_window = 1000;
        let mut store = Vec::new();
        // A long prompt pushes the estimate over window - reserve.
        let long = format!("hello {}", "x".repeat(2000));
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "prompt", "message": long})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let compacted = store.iter().any(|l| {
            serde_json::from_str::<serde_json::Value>(l.trim())
                .map(|v| v["type"] == "compacted")
                .unwrap_or(false)
        });
        assert!(compacted, "expected a compacted event: {store:?}");
        // The session file gains a compaction entry.
        let entries = runtime.get_entries().await.unwrap();
        assert!(
            entries
                .iter()
                .any(|e| matches!(e, pi_agent::session::types::Entry::Compaction { .. })),
            "expected a compaction entry"
        );
        std::env::remove_var("FAUX_API_KEY");
    }

    fn entry(
        id: &str,
        parent: Option<&str>,
        seq: u64,
        timestamp: u64,
    ) -> pi_agent::session::types::Entry {
        pi_agent::session::types::Entry::from_provisioned(
            pi_agent::session::types::EntryNoStats::ModelChange {
                id: id.to_string(),
                provider: "p".to_string(),
                model_id: "m".to_string(),
            },
            parent.map(|p| p.to_string()),
            seq,
            timestamp,
        )
    }

    #[test]
    fn tree_nests_children_and_orders_by_timestamp() {
        let entries = vec![
            entry("r", None, 1, 100),
            entry("a", Some("r"), 2, 300),
            entry("b", Some("r"), 3, 200),
        ];
        let tree = RpcRuntime::build_tree(&entries, &HashMap::new());
        let roots = tree.as_array().unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0]["entry"]["id"], "r");
        // Children sorted by entry timestamp ascending, not insertion order.
        let children = roots[0]["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0]["entry"]["id"], "b");
        assert_eq!(children[1]["entry"]["id"], "a");
    }

    #[test]
    fn tree_self_parent_and_missing_parent_are_roots() {
        let entries = vec![
            entry("s", Some("s"), 1, 100),       // self-parent
            entry("o", Some("missing"), 2, 200), // orphan
            entry("r", None, 3, 300),
        ];
        let tree = RpcRuntime::build_tree(&entries, &HashMap::new());
        let roots = tree.as_array().unwrap();
        assert_eq!(roots.len(), 3, "self-parent and orphan must be roots");
        let ids: Vec<&str> = roots
            .iter()
            .map(|n| n["entry"]["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"s"));
        assert!(ids.contains(&"o"));
        assert!(ids.contains(&"r"));
    }

    #[test]
    fn tree_emits_label_only_when_resolved() {
        let entries = vec![entry("x", None, 1, 100), entry("y", None, 2, 200)];
        let labels: HashMap<String, String> = [("y".to_string(), "My label".to_string())].into();
        let tree = RpcRuntime::build_tree(&entries, &labels);
        let roots = tree.as_array().unwrap();
        let by_id: Vec<&serde_json::Value> = roots.iter().collect();
        let x = by_id.iter().find(|n| n["entry"]["id"] == "x").unwrap();
        let y = by_id.iter().find(|n| n["entry"]["id"] == "y").unwrap();
        assert!(x.get("label").is_none(), "no label key when unresolved");
        assert_eq!(y["label"], "My label");
    }
}
