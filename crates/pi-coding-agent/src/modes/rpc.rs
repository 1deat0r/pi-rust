//! RPC mode — port of `packages/coding-agent/src/modes/rpc/rpc-mode.ts`.
//!
//! Headless operation over a JSONL stdin/stdout protocol. Receives `RpcCommand`
//! objects as one JSON per line on stdin; emits `response` records and
//! session lifecycle events as JSON lines on stdout.
//!
//! Implemented over the port's current layers: the pi-ai Models facade
//! (provider registry / catalog / auth + stream dispatch), the pi-agent agent
//! loop (with stream-event observation), and the session facade (JSONL v4
//! repo-backed). HTML export remains outside this RPC command slice; loaded
//! extension commands are discovered and dispatched through the Rust host.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_agent::agent::AgentContext;
use pi_agent::rich_agent::{
    agent_tool_result_to_partial_json, run_rich_agent_loop, AfterToolCallContext,
    AfterToolCallResult, BeforeToolCallResult, PendingMessageQueue, QueueMode, RichAgentEvent,
    RichAgentLoopConfig,
};
use pi_agent::session::jsonl::repo::CreateOptions;
use pi_agent::session::session::Session as JsonlSession;

use pi_agent::harness::events::HarnessEventBus;
use pi_agent::harness::{run_with_harness_lifecycle, HarnessTelemetryContext, SimpleModels};
use pi_agent::session::memory::{in_memory_metadata, InMemorySessionStorage};
use pi_agent::session::session::SessionStorageKind;
use pi_agent::session::state::{BranchBounds, EntryOrder, EntryQuery, ForkOptions, ForkPosition};
use pi_agent::session::types::EntryNoStats;
use pi_agent::session::JsonlSessionRepo;
use pi_ai::model::Model;
use pi_ai::models::Models;
use pi_ai::types::{AssistantMessageEvent, ContentBlock, Message, ToolResultMessage, UserContent};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::args::Args;
use crate::config;
use crate::core::extensions::types::{
    ExtensionHostAction, ExtensionHostActions, ExtensionUiRequestSink,
    ExtensionUiResponseDisposition, PendingHostAction,
};
use crate::core::extensions::{
    install_tools, load_for_mode, load_for_mode_with_reason_and_flags_and_previous,
    register_loaded_native_providers, LoadedExtensions,
};
use crate::core::settings::SettingsManager;

use super::jsonl::{serialize_json_line, JsonlLineReader};
use super::rpc_types::{failure, success, RpcCommand, RpcSessionState};

/// Max output chars before a bash result is truncated (upstream threshold).
const BASH_TRUNCATE_LIMIT: usize = 30_000;

/// The RPC mode uses the same initial active-tool policy as the stateful
/// upstream AgentSession. The registry still contains every available tool;
/// only these names are sent to the model for a turn.
const RPC_BUILTIN_TOOL_NAMES: [&str; 7] = ["bash", "read", "write", "edit", "ls", "find", "grep"];
const RPC_DEFAULT_ACTIVE_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

fn rpc_is_builtin_tool_name(name: &str) -> bool {
    RPC_BUILTIN_TOOL_NAMES.contains(&name)
}

fn rpc_select_active_tool_names(
    args: &Args,
    settings: &SettingsManager,
    tools: &[pi_agent::tools::AgentTool],
) -> Vec<String> {
    let mut active = if args.no_tools {
        Vec::new()
    } else if let Some(explicit) = args.tools.clone() {
        explicit
    } else if args.no_builtin_tools {
        tools
            .iter()
            .filter(|tool| !rpc_is_builtin_tool_name(&tool.tool.name))
            .map(|tool| tool.tool.name.clone())
            .collect()
    } else {
        settings.get_default_tools().unwrap_or_else(|| {
            RPC_DEFAULT_ACTIVE_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        })
    };

    // Extension tools are implicitly active for a normal session, matching
    // `includeAllExtensionTools: true`. An explicit --tools list remains an
    // allowlist, while --no-tools/--no-builtin-tools retain their stronger
    // policy boundaries.
    if args.tools.is_none() && !args.no_tools && !args.no_builtin_tools {
        active.extend(
            tools
                .iter()
                .filter(|tool| !rpc_is_builtin_tool_name(&tool.tool.name))
                .map(|tool| tool.tool.name.clone()),
        );
    }
    if let Some(excluded) = &args.exclude_tools {
        active.retain(|name| !excluded.iter().any(|excluded| excluded == name));
    }
    active.sort();
    active.dedup();
    active
}

type FauxResponseFactory = Box<
    dyn Fn(
            &pi_ai::types::Context,
            Option<&pi_ai::types::SimpleStreamOptions>,
            &pi_ai::providers::FauxProviderState,
            &pi_ai::model::Model,
        ) -> pi_ai::types::AssistantMessage
        + Send
        + Sync,
>;

fn rpc_user_message(text: &str, images: &[ContentBlock]) -> pi_agent::types::AgentMessage {
    let mut blocks = vec![ContentBlock::text(text)];
    blocks.extend(images.iter().cloned());
    pi_agent::types::AgentMessage::Core(Message::User(UserContent::blocks(
        blocks,
        pi_ai::types::now_ms(),
    )))
}

/// Decode an extension message returned by `before_agent_start`. Upstream
/// extensions commonly omit the optional presentation metadata on custom
/// messages, while the Rust wire type keeps those fields explicit. Fill only
/// those defaults; malformed messages are ignored by the RPC adapter just as
/// an extension result with an unusable message would be by the JS session.
fn rpc_extension_agent_message(value: serde_json::Value) -> Option<pi_agent::types::AgentMessage> {
    if let Ok(message) = serde_json::from_value::<pi_agent::types::AgentMessage>(value.clone()) {
        return Some(message);
    }
    let serde_json::Value::Object(mut object) = value else {
        return None;
    };
    if object.get("role").and_then(serde_json::Value::as_str) != Some("custom") {
        return None;
    }
    object
        .entry("customType".to_string())
        .or_insert_with(|| serde_json::Value::String("custom".to_string()));
    object
        .entry("content".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    object
        .entry("display".to_string())
        .or_insert(serde_json::Value::Bool(true));
    object
        .entry("timestamp".to_string())
        .or_insert_with(|| serde_json::json!(pi_ai::types::now_ms()));
    serde_json::from_value(serde_json::Value::Object(object)).ok()
}

/// Convert the stateful extension hook boundary into the rich-loop hook used
/// by RPC. The runner is retained by the detached prompt config so each turn
/// invokes the same extension instance in registration order.
fn rpc_before_tool_call_hook(
    runner: Arc<crate::core::extensions::ExtensionRunner>,
) -> pi_agent::rich_agent::BeforeToolCallHook {
    Arc::new(move |context, _signal| {
        let runner = runner.clone();
        Box::pin(async move {
            let event = serde_json::json!({
                "type": "tool_call",
                "toolName": context.tool_name.clone(),
                "toolCallId": context.tool_call_id.clone(),
                "input": context.args.clone(),
            });
            let result = runner.emit_tool_call(&event)?;
            if let Some(updated_args) = result.get("input").or_else(|| result.get("args")).cloned()
            {
                context.args = updated_args;
            }
            Some(BeforeToolCallResult {
                block: result
                    .get("block")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                reason: result
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                terminate: result
                    .get("terminate")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            })
        })
    })
}

/// Convert the rich-loop tool result into the extension runner's upstream
/// hook shape, then apply the returned content/details/usage/error overrides
/// through the runner's sequential result patch implementation.
fn rpc_after_tool_call_hook(
    runner: Arc<crate::core::extensions::ExtensionRunner>,
) -> pi_agent::rich_agent::AfterToolCallHook {
    Arc::new(move |context: AfterToolCallContext, _signal| {
        let runner = runner.clone();
        Box::pin(async move {
            let usage = match &context.result {
                ToolResultMessage::ToolResult { usage, .. } => usage.clone(),
            };
            let event = serde_json::json!({
                "type": "tool_result",
                "toolName": context.tool_name,
                "toolCallId": context.tool_call_id,
                "input": context.args,
                "content": context.result.content(),
                "details": context.result.details(),
                "isError": context.is_error,
                "usage": usage,
            });
            let result = runner.emit_tool_result(event)?;
            let content = result.get("content").and_then(rpc_content_blocks);
            let details = result.get("details").cloned();
            let usage = result
                .get("usage")
                .filter(|value| !value.is_null())
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
            let added_tool_names = result
                .get("addedToolNames")
                .and_then(serde_json::Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                });
            let terminate = result.get("terminate").and_then(serde_json::Value::as_bool);
            if content.is_none()
                && details.is_none()
                && usage.is_none()
                && is_error.is_none()
                && added_tool_names.is_none()
                && terminate.is_none()
            {
                return None;
            }
            Some(AfterToolCallResult {
                content,
                details,
                usage,
                added_tool_names,
                is_error,
                terminate,
            })
        })
    })
}

fn rpc_content_blocks(value: &serde_json::Value) -> Option<Vec<ContentBlock>> {
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<Vec<ContentBlock>>(value.clone())
        .ok()
        .or_else(|| value.as_str().map(|text| vec![ContentBlock::text(text)]))
}

/// Decode the wire-level `ImageContent[]` shape used by prompt, steer, and
/// follow_up. Keep the validation at the RPC boundary so malformed commands
/// receive a normal failure response instead of being silently dropped from
/// the model context.
fn rpc_images(command: &RpcCommand) -> Result<Vec<ContentBlock>, String> {
    let Some(value) = command.value.get("images") else {
        return Ok(Vec::new());
    };
    let Some(images) = value.as_array() else {
        return Err("images must be an array".to_string());
    };
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let data = image
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("images[{index}] is missing data"))?;
            let mime_type = image
                .get("mimeType")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("images[{index}] is missing mimeType"))?;
            Ok(ContentBlock::image(data, mime_type))
        })
        .collect()
}

fn resource_contains_path(
    resource: &crate::core::extensions::DiscoveredResource,
    loaded_path: &str,
    cwd: &str,
) -> bool {
    let resource_path = PathBuf::from(resource.resolved_path(cwd));
    let loaded_path = crate::core::extensions::loader::resolve_relative_path(loaded_path, cwd);
    loaded_path == resource_path || loaded_path.starts_with(&resource_path)
}

fn apply_extension_skill_source_info(
    skills: &mut [crate::core::skills::Skill],
    resources: &[crate::core::extensions::DiscoveredResource],
    cwd: &str,
) {
    for skill in skills {
        if let Some(resource) = resources
            .iter()
            .find(|resource| resource_contains_path(resource, &skill.file_path, cwd))
        {
            let mut source_info = resource.source_info.clone();
            source_info.path = skill.file_path.clone();
            skill.source_info = source_info;
        }
    }
}

fn load_rpc_skills(
    args: &Args,
    cwd: &str,
    agent_dir: &str,
    settings: &SettingsManager,
    resources: &crate::core::extensions::ResourceDiscovery,
) -> Vec<crate::core::skills::Skill> {
    if args.no_skills {
        return Vec::new();
    }
    let mut skill_paths = settings.get_skill_paths();
    skill_paths.extend(args.skills.iter().cloned());
    skill_paths.extend(resources.resolved_skill_paths(cwd));
    let (mut skills, diagnostics) =
        crate::core::skills::load_skills(crate::core::skills::LoadSkillsOptions {
            cwd: cwd.to_string(),
            agent_dir: agent_dir.to_string(),
            skill_paths,
        });
    apply_extension_skill_source_info(&mut skills, &resources.skill_resources, cwd);
    for diagnostic in diagnostics {
        tracing::warn!(
            path = ?diagnostic.path,
            message = %diagnostic.message,
            "skill load diagnostic"
        );
    }
    skills
}

fn load_rpc_prompt_templates(
    args: &Args,
    cwd: &str,
    agent_dir: &str,
    settings: &SettingsManager,
    resources: &crate::core::extensions::ResourceDiscovery,
) -> Vec<crate::core::prompt_templates::PromptTemplate> {
    if args.no_prompt_templates {
        return Vec::new();
    }
    let mut rpc_args = args.clone();
    let mut prompt_paths = settings.get_prompt_template_paths();
    prompt_paths.extend(args.prompt_templates.iter().cloned());
    rpc_args.prompt_templates = prompt_paths;
    crate::run::load_prompt_templates_for_run(&rpc_args, cwd, Path::new(agent_dir), resources)
}

fn source_info_json(source_info: &crate::core::extensions::SourceInfo) -> serde_json::Value {
    let mut object = serde_json::Map::from_iter([
        (
            "path".to_string(),
            serde_json::Value::String(source_info.path.clone()),
        ),
        (
            "source".to_string(),
            serde_json::Value::String(source_info.source.clone()),
        ),
        (
            "scope".to_string(),
            serde_json::Value::String(source_info.scope.clone()),
        ),
        (
            "origin".to_string(),
            serde_json::Value::String(source_info.origin.clone()),
        ),
    ]);
    if let Some(base_dir) = &source_info.base_dir {
        object.insert(
            "baseDir".to_string(),
            serde_json::Value::String(base_dir.clone()),
        );
    }
    serde_json::Value::Object(object)
}

fn rpc_slash_command_json(
    name: impl Into<String>,
    description: Option<String>,
    source: &str,
    source_info: &crate::core::extensions::SourceInfo,
) -> serde_json::Value {
    let mut object = serde_json::Map::from_iter([
        ("name".to_string(), serde_json::Value::String(name.into())),
        (
            "source".to_string(),
            serde_json::Value::String(source.to_string()),
        ),
        ("sourceInfo".to_string(), source_info_json(source_info)),
    ]);
    if let Some(description) = description {
        object.insert(
            "description".to_string(),
            serde_json::Value::String(description),
        );
    }
    serde_json::Value::Object(object)
}

fn queue_mode(value: &str) -> QueueMode {
    if value == "all" {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

fn normalized_queue_mode(value: &str) -> String {
    match value {
        "all" => "all".to_string(),
        "one-at-a-time" => "one-at-a-time".to_string(),
        _ => "one-at-a-time".to_string(),
    }
}

fn configured_thinking_level(
    settings: &SettingsManager,
    model: &Model,
) -> pi_ai::types::ModelThinkingLevel {
    let raw = settings
        .get_model_thinking_level(&model.provider, &model.id)
        .or_else(|| settings.get_default_thinking_level())
        .unwrap_or("off");
    let requested = raw
        .parse::<pi_ai::types::ModelThinkingLevel>()
        .unwrap_or(pi_ai::types::ModelThinkingLevel::Off);
    pi_ai::model::clamp_thinking_level(model, requested)
}

fn thinking_level_for_request(
    level: pi_ai::types::ModelThinkingLevel,
) -> Option<pi_ai::types::ThinkingLevel> {
    match level {
        pi_ai::types::ModelThinkingLevel::Off => None,
        pi_ai::types::ModelThinkingLevel::Minimal => Some(pi_ai::types::ThinkingLevel::Minimal),
        pi_ai::types::ModelThinkingLevel::Low => Some(pi_ai::types::ThinkingLevel::Low),
        pi_ai::types::ModelThinkingLevel::Medium => Some(pi_ai::types::ThinkingLevel::Medium),
        pi_ai::types::ModelThinkingLevel::High => Some(pi_ai::types::ThinkingLevel::High),
        pi_ai::types::ModelThinkingLevel::Xhigh => Some(pi_ai::types::ThinkingLevel::Xhigh),
        pi_ai::types::ModelThinkingLevel::Max => Some(pi_ai::types::ThinkingLevel::Max),
    }
}

fn thinking_budgets(settings: &SettingsManager) -> Option<pi_ai::types::ThinkingBudgets> {
    let values = settings.get_thinking_budgets()?;
    let budget = |name: &str| values.get(name).and_then(serde_json::Value::as_u64);
    Some(pi_ai::types::ThinkingBudgets {
        minimal: budget("minimal"),
        low: budget("low"),
        medium: budget("medium"),
        high: budget("high"),
    })
}

/// Return the error that the upstream `AgentSession.prompt()` reports when a
/// non-queued prompt cannot pass model/auth preflight.  RPC emits the prompt
/// response only after this check; doing it at the mode boundary is important
/// because detached prompt workers cannot retroactively replace a success line
/// with the single correlated failure that upstream sends.
fn rpc_prompt_preflight_error(
    provider: &str,
    api_key: Option<&str>,
    has_configured_auth: bool,
    oauth_capable: bool,
) -> Option<String> {
    if api_key.is_some_and(|key| !key.is_empty()) || has_configured_auth {
        return None;
    }
    if oauth_capable {
        return Some(format!(
            "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate."
        ));
    }
    Some(crate::core::auth_guidance::format_no_api_key_found_message(
        provider,
    ))
}

/// Merge the coding-agent runtime's provider settings into a request-specific
/// simple-stream option set. Compaction supplies request-local values such as
/// max tokens and a fresh session id; those values must win over the runtime
/// defaults.
fn apply_provider_defaults(
    defaults: &pi_ai::types::SimpleStreamOptions,
    options: &mut pi_ai::types::SimpleStreamOptions,
) {
    if options.base.base.api_key.is_none() {
        options.base.base.api_key = defaults.base.base.api_key.clone();
    }
    if options.base.base.timeout_ms.is_none() {
        options.base.base.timeout_ms = defaults.base.base.timeout_ms;
    }
    if options.base.base.max_retries.is_none() {
        options.base.base.max_retries = defaults.base.base.max_retries;
    }
    if options.base.base.max_retry_delay_ms.is_none() {
        options.base.base.max_retry_delay_ms = defaults.base.base.max_retry_delay_ms;
    }
    if options.base.transport.is_none() {
        options.base.transport = defaults.base.transport.clone();
    }
    if options.base.session_id.is_none() {
        options.base.session_id = defaults.base.session_id.clone();
    }
    if options.base.websocket_connect_timeout_ms.is_none() {
        options.base.websocket_connect_timeout_ms = defaults.base.websocket_connect_timeout_ms;
    }
    if options.thinking_budgets.is_none() {
        options.thinking_budgets = defaults.thinking_budgets.clone();
    }
}

/// The RPC runtime: owns the current model/session and executes commands.
pub struct RpcRuntime {
    pub cwd: String,
    pub agent_dir: String,
    /// The immutable CLI extension configuration is retained so a session
    /// transition can construct the same extension set as startup.
    extension_args: Args,
    pub settings: SettingsManager,
    pub models: Models,
    /// Shared faux registration used by headless mode so prompt, compaction,
    /// and deferred operations all use the provider facade.
    faux_core: Option<pi_ai::providers::FauxProviderCore>,
    /// Faux's scripted prompt queue is intentionally separate from the
    /// compaction summary queue, matching the established RPC golden wire.
    faux_summary_core: Option<pi_ai::providers::FauxProviderCore>,
    api_key: Option<String>,
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
    /// Whether the currently bound session is durable. `--no-session` starts
    /// false, while an explicit runtime session switch can intentionally bind
    /// a persistent session just as the upstream runtime does.
    pub session_persistence: bool,
    pub auto_compaction_enabled: bool,
    pub auto_retry_enabled: bool,
    pub messages: Vec<pi_agent::types::AgentMessage>,
    pub repo: JsonlSessionRepo<pi_agent::fs::StdFileSystem>,
    pub session: JsonlSession<pi_agent::fs::StdFileSystem>,
    /// Bash results that finish while an agent run is active. Upstream defers
    /// these messages until the run settles so they cannot split an assistant
    /// tool-call/tool-result sequence.
    pending_bash_messages: Vec<pi_agent::types::AgentMessage>,
    /// Session events produced by automatic compaction while the prompt
    /// worker is detached; flushed when the run settles.
    pending_session_events: Vec<String>,
    /// One cancellation flag per standalone RPC bash task. `abort_bash` must
    /// cancel every current command without allowing a new command to clear
    /// an older command's cancellation state.
    bash_abort_flags: Vec<Arc<AtomicBool>>,
    pub run_lock: Arc<Mutex<bool>>,
    pub abort_bash: Arc<AtomicBool>,
    pub abort_signal: Arc<AtomicBool>,
    pub abort_retry_signal: Arc<AtomicBool>,
    pub steering_queue: Arc<Mutex<PendingMessageQueue>>,
    pub follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    pub system_prompt: Option<String>,
    pub tools_enabled: bool,
    pub builtin_tools_enabled: bool,
    /// Prompt and skill resources are retained because RPC `get_commands`
    /// exposes the same resource-backed slash command catalog as upstream.
    pub prompt_templates: Vec<crate::core::prompt_templates::PromptTemplate>,
    pub skills: Vec<crate::core::skills::Skill>,
    /// The live active-tool allowlist used to construct the next prompt
    /// context. It is initialized from CLI/settings policy and replaced when
    /// an extension `setActiveTools` request is applied.
    active_tool_names: Option<Vec<String>>,
    /// One extension runtime is shared by all prompt contexts for this RPC
    /// session. Prompt tool closures retain the runner while detached.
    pub loaded_extensions: LoadedExtensions,
    /// Sink installed by the RPC loop. It is retained so a session/reload
    /// replacement can attach the same output path to the new host broker.
    ui_request_sink: Option<ExtensionUiRequestSink>,
}

impl Drop for RpcRuntime {
    fn drop(&mut self) {
        // Detached prompt workers can outlive the input loop on EOF. Mark the
        // shared runtime stale before the owned field is dropped so late
        // callbacks fail safely while their process is being torn down.
        let _ = self.loaded_extensions.runner.emit_session_shutdown("quit");
        self.loaded_extensions
            .runner
            .invalidate(Some("rpc mode shutdown"));
    }
}

/// Everything a prompt worker needs after it has been detached from the
/// mutable RPC runtime. The session and runtime remain available to the input
/// loop while this run is streaming.
struct RpcPromptRun {
    prompts: Vec<pi_agent::types::AgentMessage>,
    context: AgentContext,
    config: RichAgentLoopConfig,
    provider_uses_oauth: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForkSessionResult {
    selected_text: Option<String>,
    cancelled: bool,
}

/// A message-end record plus the session-only termination marker carried by
/// the upstream harness entry. `terminate` is deliberately not part of the
/// model-facing `ToolResultMessage`; it is persisted alongside that message
/// so recovery can reconstruct the completed tool batch.
#[derive(Debug, Clone)]
struct PersistedRpcMessage {
    message: pi_agent::types::AgentMessage,
    terminate: bool,
}

fn plain_persisted_messages(
    messages: Vec<pi_agent::types::AgentMessage>,
) -> Vec<PersistedRpcMessage> {
    messages
        .into_iter()
        .map(|message| PersistedRpcMessage {
            message,
            terminate: false,
        })
        .collect()
}

fn capture_persisted_rpc_event(
    event: &RichAgentEvent,
    pending_terminations: &mut HashSet<String>,
    persisted_messages: &mut Vec<PersistedRpcMessage>,
) {
    match event {
        RichAgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            ..
        } => {
            if result.terminate {
                pending_terminations.insert(tool_call_id.clone());
            } else {
                pending_terminations.remove(tool_call_id);
            }
        }
        RichAgentEvent::MessageEnd { message } => {
            let terminate = match message {
                pi_agent::types::AgentMessage::Core(Message::ToolResult(result)) => {
                    pending_terminations.remove(result.tool_call_id())
                }
                _ => false,
            };
            persisted_messages.push(PersistedRpcMessage {
                message: message.clone(),
                terminate,
            });
        }
        _ => {}
    }
}

struct RpcPromptResult {
    /// Messages that remain in the live agent context after retry handling.
    /// Intermediate retry failures are intentionally absent.
    new_messages: Vec<pi_agent::types::AgentMessage>,
    /// Every message-end record from the run, including intermediate retry
    /// failures. These are the durable session history.
    persisted_messages: Vec<PersistedRpcMessage>,
}

enum RpcPromptTaskMessage {
    Event(String),
    Finished(RpcPromptResult),
    ExtensionCommandFinished {
        id: Option<String>,
        command: String,
        result: Result<Option<serde_json::Value>, String>,
    },
}

struct RpcBashTaskResult {
    id: Option<String>,
    command: String,
    exclude_from_context: Option<bool>,
    abort: Arc<AtomicBool>,
    result: serde_json::Value,
}

struct RpcBashUpdate {
    id: Option<String>,
    delta: String,
}

enum RpcTaskMessage {
    Prompt(RpcPromptTaskMessage),
    BashUpdate(RpcBashUpdate),
    Bash(RpcBashTaskResult),
    ExtensionUiRequest(String),
}

#[cfg(test)]
fn serialize_rpc_prompt_event(event: RichAgentEvent) -> Option<String> {
    serialize_rpc_prompt_event_with_auth(event, None, false)
}

pub(crate) fn serialize_rpc_prompt_event_with_auth(
    event: RichAgentEvent,
    provider: Option<&str>,
    provider_uses_oauth: bool,
) -> Option<String> {
    match event {
        RichAgentEvent::AgentStart => Some(serialize_json_line(&serde_json::json!({
            "type": "agent_start",
        }))),
        RichAgentEvent::AgentEnd { messages } => Some(serialize_json_line(&serde_json::json!({
            "type": "agent_end",
            "messages": messages,
            // Provider retries are handled inside this Rust agent run; there
            // is no second session-level retry after this event.
            "willRetry": false,
        }))),
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
        RichAgentEvent::TurnStart => Some(serialize_json_line(&serde_json::json!({
            "type": "turn_start",
        }))),
        RichAgentEvent::TurnEnd {
            message,
            tool_results,
        } => Some(serialize_json_line(&serde_json::json!({
            "type": "turn_end",
            "message": message,
            "toolResults": tool_results,
        }))),
        RichAgentEvent::MessageStart { message } => Some(serialize_json_line(&serde_json::json!({
            "type": "message_start",
            "message": message,
        }))),
        RichAgentEvent::MessageUpdate {
            mut assistant_message_event,
            ..
        } => {
            if let (Some(provider), AssistantMessageEvent::Error { error_message, .. }) =
                (provider, &mut assistant_message_event)
            {
                crate::core::auth_guidance::rewrite_assistant_error(
                    error_message,
                    provider,
                    provider_uses_oauth,
                );
            }
            Some(serialize_json_line(&to_json_message_update(
                &assistant_message_event,
            )))
        }
        RichAgentEvent::MessageEnd { message } => Some(serialize_json_line(&serde_json::json!({
            "type": "message_end",
            "message": message,
        }))),
        RichAgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(serialize_json_line(&serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
        }))),
        RichAgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => Some(serialize_json_line(&serde_json::json!({
            "type": "tool_execution_update",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
            "partialResult": partial_result,
        }))),
        RichAgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Some(serialize_json_line(&serde_json::json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "result": agent_tool_result_to_partial_json(&result),
            "isError": is_error,
        }))),
    }
}

fn compaction_retry_callbacks(
    events: Arc<Mutex<Vec<serde_json::Value>>>,
    reason: &'static str,
) -> pi_ai::utils::RetryCallbacks<'static> {
    let scheduled_events = events.clone();
    let attempt_events = events.clone();
    let finished_events = events;
    pi_ai::utils::RetryCallbacks {
        on_retry_scheduled: Some(Box::new(
            move |attempt, max_attempts, delay_ms, error_message| {
                scheduled_events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(serde_json::json!({
                        "type": "summarization_retry_scheduled",
                        "attempt": attempt,
                        "maxAttempts": max_attempts,
                        "delayMs": delay_ms,
                        "errorMessage": error_message,
                    }));
            },
        )),
        on_retry_attempt_start: Some(Box::new(move || {
            attempt_events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(serde_json::json!({
                    "type": "summarization_retry_attempt_start",
                    "source": "compaction",
                    "reason": reason,
                }));
        })),
        on_retry_finished: Some(Box::new(move |_success, _attempt, _final_error| {
            finished_events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(serde_json::json!({"type": "summarization_retry_finished"}));
        })),
    }
}

fn append_recorded_events(store: &mut Vec<String>, events: &Arc<Mutex<Vec<serde_json::Value>>>) {
    for event in events
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drain(..)
    {
        store.push(serialize_json_line(&event));
    }
}

fn emit_rpc_extension_event(
    runner: &crate::core::extensions::ExtensionRunner,
    event_type: &str,
    payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    if !runner.has_handlers(event_type) {
        return None;
    }
    match runner.emit(event_type, payload) {
        Ok(result) => result,
        Err(errors) => {
            for error in errors {
                tracing::warn!(
                    extension = %error.extension_path,
                    event = %error.event,
                    error = %error.error,
                    "RPC extension lifecycle handler failed"
                );
            }
            None
        }
    }
}

fn rpc_compaction_preparation_value(
    preparation: &pi_agent::harness::compaction::CompactionPreparation,
    first_kept_entry_id: Option<&str>,
) -> serde_json::Value {
    let file_ops = &preparation.file_ops;
    serde_json::json!({
        "firstKeptEntryId": first_kept_entry_id.unwrap_or_default(),
        "messagesToSummarize": preparation.messages_to_summarize,
        "turnPrefixMessages": preparation.turn_prefix_messages,
        "isSplitTurn": preparation.is_split_turn,
        "tokensBefore": preparation.tokens_before,
        "previousSummary": preparation.previous_summary,
        "fileOps": {
            "read": file_ops.read.iter().cloned().collect::<Vec<_>>(),
            "written": file_ops.written.iter().cloned().collect::<Vec<_>>(),
            "edited": file_ops.edited.iter().cloned().collect::<Vec<_>>(),
        },
        "settings": {
            "enabled": preparation.settings.enabled,
            "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        },
    })
}

fn rpc_session_before_compact(
    runner: &crate::core::extensions::ExtensionRunner,
    preparation: &pi_agent::harness::compaction::CompactionPreparation,
    entries: &[pi_agent::session::types::Entry],
    first_kept_entry_id: Option<&str>,
    custom_instructions: Option<&str>,
    reason: &str,
    will_retry: bool,
) -> Option<serde_json::Value> {
    let branch_entries = serde_json::to_value(entries).unwrap_or(serde_json::Value::Array(vec![]));
    let payload = serde_json::json!({
        "type": "session_before_compact",
        "preparation": rpc_compaction_preparation_value(preparation, first_kept_entry_id),
        "branchEntries": branch_entries,
        "customInstructions": custom_instructions,
        "reason": reason,
        "willRetry": will_retry,
    });
    emit_rpc_extension_event(runner, "session_before_compact", &payload)
}

fn rpc_session_compact(
    runner: &crate::core::extensions::ExtensionRunner,
    entry: &pi_agent::session::types::Entry,
    from_extension: bool,
    reason: &str,
    will_retry: bool,
) {
    let payload = serde_json::json!({
        "type": "session_compact",
        "compactionEntry": entry,
        "fromExtension": from_extension,
        "reason": reason,
        "willRetry": will_retry,
    });
    let _ = emit_rpc_extension_event(runner, "session_compact", &payload);
}

fn rpc_session_compact_failed(
    runner: &crate::core::extensions::ExtensionRunner,
    reason: &str,
    aborted: bool,
    from_extension: bool,
    will_retry: bool,
) {
    let payload = serde_json::json!({
        "type": "session_compact_failed",
        "reason": reason,
        "aborted": aborted,
        "fromExtension": from_extension,
        "willRetry": will_retry,
    });
    let _ = emit_rpc_extension_event(runner, "session_compact_failed", &payload);
}

type RpcExtensionCompactionResult = (
    String,
    String,
    u64,
    Vec<pi_agent::types::AgentMessage>,
    Option<serde_json::Value>,
    Option<pi_ai::types::Usage>,
);

fn rpc_extension_compaction_result(
    value: Option<&serde_json::Value>,
    preparation: &pi_agent::harness::compaction::CompactionPreparation,
    entries: &[pi_agent::session::types::Entry],
) -> Option<RpcExtensionCompactionResult> {
    let compaction = value?.get("compaction")?.as_object()?;
    let summary = compaction.get("summary")?.as_str()?.to_string();
    let first_kept_entry_id = compaction
        .get("firstKeptEntryId")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let tokens_before = compaction
        .get("tokensBefore")
        .and_then(serde_json::Value::as_u64)?;
    let retained_tail = entries
        .iter()
        .position(|entry| entry.id() == first_kept_entry_id)
        .map(|index| {
            entries[index..]
                .iter()
                .filter_map(|entry| entry.as_message().cloned())
                .collect::<Vec<_>>()
        })
        .filter(|messages| !messages.is_empty())
        .unwrap_or_else(|| preparation.retained_tail.clone());
    let details = compaction.get("details").cloned();
    let usage = compaction
        .get("usage")
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    Some((
        summary,
        first_kept_entry_id,
        tokens_before,
        retained_tail,
        details,
        usage,
    ))
}

fn serialize_rpc_bash_update(id: Option<&str>, delta: &str) -> String {
    let mut event = serde_json::json!({
        "type": "bash_execution_update",
        "delta": delta,
    });
    if let Some(id) = id {
        event["id"] = serde_json::Value::String(id.to_string());
    }
    serialize_json_line(&event)
}

async fn run_rpc_prompt(run: RpcPromptRun, events: UnboundedSender<RpcPromptTaskMessage>) {
    let RpcPromptRun {
        prompts,
        mut context,
        config,
        provider_uses_oauth,
    } = run;
    let provider = config.model.provider.clone();
    let persisted_messages = Arc::new(Mutex::new(Vec::new()));
    let persisted_for_loop = persisted_messages.clone();
    let events_for_loop = events.clone();
    let mut pending_terminations = HashSet::new();
    let mut emit: Box<dyn FnMut(RichAgentEvent) + Send> = Box::new(move |event| {
        capture_persisted_rpc_event(
            &event,
            &mut pending_terminations,
            &mut persisted_for_loop
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        if let Some(line) =
            serialize_rpc_prompt_event_with_auth(event, Some(&provider), provider_uses_oauth)
        {
            let _ = events_for_loop.send(RpcPromptTaskMessage::Event(line));
        }
    });
    let telemetry = HarnessTelemetryContext::default();
    let mut lifecycle = HarnessEventBus::new();
    let session_id = config
        .session_id
        .clone()
        .unwrap_or_else(|| "rpc-mode".to_string());
    let new_messages = run_with_harness_lifecycle(
        &telemetry,
        &mut lifecycle,
        "main",
        &session_id,
        String::new(),
        move |_span| async move {
            Ok(run_rich_agent_loop(prompts, &mut context, &config, &mut emit).await)
        },
    )
    .await
    .unwrap_or_default();
    let persisted_messages = std::mem::take(
        &mut *persisted_messages
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
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
    /// Attach the live RPC output path to the current extension host. Startup
    /// lifecycle callbacks run before this exists, so their dialogs are
    /// rejected safely and their fire-and-forget requests remain queued until
    /// the mode enters its stdin/stdout loop.
    pub fn bind_ui_request_sink(&mut self, sink: ExtensionUiRequestSink) {
        self.ui_request_sink = Some(sink.clone());
        self.loaded_extensions.host.set_ui_request_sink(sink);
        self.loaded_extensions.host.set_ui_enabled(true);
    }

    /// Build a fresh runtime (mirrors the run path's model/session wiring).
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub async fn new(args: &Args, settings: SettingsManager) -> Result<Self, String> {
        let cwd = config::cwd();
        let agent_dir = config::get_agent_dir().display().to_string();
        let models = crate::core::model_registry::builtin_models();
        let api_key = args
            .api_key
            .clone()
            .and_then(|key| config::nonempty_env_value(Some(key)))
            .or_else(|| config::nonempty_env_value(std::env::var(config::ENV_KEY).ok()));
        let steering_mode = normalized_queue_mode(settings.get_steering_mode());
        let follow_up_mode = normalized_queue_mode(settings.get_follow_up_mode());
        let auto_compaction_enabled = settings.get_compaction_enabled();
        let auto_retry_enabled = settings.get_retry_enabled();

        let mut provider = crate::run::resolve_run_provider(args.provider.as_deref(), &settings);
        let model_hint = crate::run::resolve_run_model(
            args.model.as_deref(),
            &settings,
            !crate::run::has_explicit_provider(args.provider.as_deref()),
            Some(&provider),
        );

        // Session repo + initial session.
        let session_root = crate::run::resolve_session_root(args, Some(&settings));
        let session_persistence = !args.no_session;
        let selects_existing =
            args.continue_session || args.resume || args.session.is_some() || args.fork.is_some();
        if !session_persistence && selects_existing {
            return Err(
                "--continue, --resume, --session, and --fork require session persistence"
                    .to_string(),
            );
        }
        if session_persistence {
            std::fs::create_dir_all(&session_root)
                .map_err(|e| format!("create session dir: {e}"))?;
            crate::core::session_migration::migrate_legacy_sessions_in_root(std::path::Path::new(
                &session_root,
            ))
            .map_err(|e| format!("migrate legacy sessions: {e}"))?;
        }
        let mut repo = JsonlSessionRepo::new(pi_agent::fs::StdFileSystem::new(&cwd), &session_root);
        let source_selector = args.fork.as_deref().or(args.session.as_deref());
        let mut session = if !session_persistence {
            let id = args
                .session_id
                .clone()
                .or_else(|| std::env::var(config::ENV_SESSION_ID).ok())
                .unwrap_or_else(|| format!("rpc-{}", pi_agent::session::new_id()));
            let mut metadata = in_memory_metadata(id, None);
            metadata.cwd = cwd.clone();
            let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(metadata)));
            JsonlSession::from_in_memory(storage)
        } else if let Some(selector) = source_selector {
            let selected_path = crate::run::resolve_session_selector_path(selector, &cwd);
            if selected_path.is_file() {
                crate::core::session_migration::migrate_legacy_session_file(&selected_path)
                    .map_err(|e| format!("migrate selected session: {e}"))?;
            }
            let source = crate::run::resolve_session_metadata(&repo, selector, &cwd).await?;
            if args.fork.is_some() {
                repo.fork(
                    &source,
                    CreateOptions {
                        id: args
                            .session_id
                            .clone()
                            .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
                        cwd: cwd.clone(),
                        parent_session_id: None,
                        metadata: None,
                        fork_options: ForkOptions::Tree,
                    },
                )
                .await
                .map_err(|e| format!("fork session {}: {e}", source.id))?
            } else {
                repo.open(&source)
                    .await
                    .map_err(|e| format!("open session {}: {e}", source.id))?
            }
        } else if args.continue_session || args.resume {
            let mut sessions = repo
                .list(Some(&cwd))
                .await
                .map_err(|e| format!("list sessions: {e}"))?;
            sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_at));
            let source = sessions.into_iter().next().ok_or_else(|| {
                if args.resume {
                    "no sessions found to resume in this directory".to_string()
                } else {
                    "no previous session found to continue in this directory".to_string()
                }
            })?;
            repo.open(&source)
                .await
                .map_err(|e| format!("open session {}: {e}", source.id))?
        } else {
            repo.create(CreateOptions {
                id: args
                    .session_id
                    .clone()
                    .or_else(|| std::env::var(config::ENV_SESSION_ID).ok()),
                cwd: cwd.clone(),
                parent_session_id: None,
                metadata: None,
                fork_options: ForkOptions::Tree,
            })
            .await
            .map_err(|e| format!("create session: {e}"))?
        };
        if let Some(name) = &args.name {
            let normalized_name = crate::run::normalize_session_name_value(name);
            if normalized_name.is_empty() {
                return Err("--name requires a non-empty value".to_string());
            }
            session
                .set_name(Some(&normalized_name))
                .await
                .map_err(|e| format!("set session name: {e}"))?;
        }
        let meta = session.get_metadata().await;
        let session_path = session_persistence.then(|| meta.path.clone());
        let session_id = meta.id.clone();
        let session_name = session.get_name().await;
        let initial_thinking_level = settings
            .get_default_thinking_level()
            .unwrap_or("off")
            .to_string();

        let loaded_extensions = load_for_mode(
            args,
            &settings,
            &cwd,
            &agent_dir,
            "rpc",
            true,
            session_name.clone(),
            initial_thinking_level,
        );
        for error in &loaded_extensions.errors {
            tracing::warn!(path = %error.path, error = %error.error, "failed to load RPC extension");
        }

        register_loaded_native_providers(&models, &loaded_extensions)
            .map_err(|error| format!("failed to register RPC native providers: {error}"))?;

        crate::core::model_runtime::register_llama_provider_if_selected(
            &models,
            &provider,
            !args.offline && !config::env_flag(config::ENV_OFFLINE),
        )
        .await?;

        let faux_core = if provider == "faux" {
            // faux is intentionally not in the builtin registry; register a
            // scripted provider so facade-backed paths (RPC compact summary
            // generation) resolve it like any real provider.
            use pi_ai::providers::{
                faux_assistant_message, FauxAssistantOptions, FauxResponseStep,
                RegisterFauxProviderOptions,
            };
            let core = crate::core::model_runtime::register_faux_provider(
                &models,
                &RegisterFauxProviderOptions::default(),
            );
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text(
                    "Compaction summary: history retained",
                )],
                FauxAssistantOptions::default(),
            ))]);
            Some(core)
        } else {
            None
        };
        let faux_summary_core = if faux_core.is_some() {
            let core = pi_ai::providers::FauxProviderCore::new(
                &pi_ai::providers::RegisterFauxProviderOptions::default(),
            );
            core.set_responses(vec![pi_ai::providers::FauxResponseStep::Message(
                pi_ai::providers::faux_assistant_message(
                    vec![pi_ai::types::ContentBlock::text(
                        "Compaction summary: history retained",
                    )],
                    pi_ai::providers::FauxAssistantOptions::default(),
                ),
            )]);
            Some(core)
        } else {
            None
        };
        let (scoped_models, scope_diagnostics) =
            crate::run::resolve_effective_model_scope(args, &settings, &models.get_models(None));
        for diagnostic in scope_diagnostics {
            eprintln!("Warning: {}", diagnostic.message);
        }
        let has_explicit_model = args.model.as_deref().is_some_and(|model| !model.is_empty());
        let initial_scoped_model = if !has_explicit_model && !selects_existing {
            scoped_models.first().map(|scoped| scoped.model.clone())
        } else {
            None
        };
        if let Some(scoped_model) = &initial_scoped_model {
            provider = scoped_model.provider.clone();
        }
        // A scoped pattern may select a provider different from the ambient
        // auth fallback. Re-run the local provider registration after that
        // choice so provider/model compatibility is checked against the
        // selected model, not the pre-scope fallback.
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
        let model = if let Some(scoped_model) = initial_scoped_model {
            scoped_model
        } else if provider == "faux" {
            let core = faux_core.as_ref().expect("faux core registered");
            match model_hint.as_deref() {
                Some(hint) => core
                    .get_model(Some(hint.rsplit('/').next().unwrap_or(hint)))
                    .cloned()
                    .ok_or_else(|| format!("unknown faux model {hint:?}"))?,
                None => core
                    .models
                    .first()
                    .cloned()
                    .ok_or_else(|| "no faux model".to_string())?,
            }
        } else {
            crate::core::model_runtime::resolve_run_model_for_provider(
                &models,
                &provider,
                model_hint.as_deref(),
            )?
        };
        crate::core::model_runtime::refresh_provider_oauth_if_needed(&models, &provider).await?;
        let thinking_level = configured_thinking_level(&settings, &model);
        let prompt_templates = load_rpc_prompt_templates(
            args,
            &cwd,
            &agent_dir,
            &settings,
            &loaded_extensions.resources,
        );
        let skills = load_rpc_skills(
            args,
            &cwd,
            &agent_dir,
            &settings,
            &loaded_extensions.resources,
        );
        loaded_extensions
            .host
            .set_thinking_level(thinking_level.as_str());

        let system_prompt = Some(crate::run::assemble_run_system_prompt(
            args,
            &cwd,
            std::path::Path::new(&agent_dir),
            &settings,
            &loaded_extensions.resources,
        ));
        let mut runtime = Self {
            cwd,
            agent_dir,
            extension_args: args.clone(),
            settings,
            models,
            faux_core,
            faux_summary_core,
            api_key,
            provider,
            model,
            thinking_level,
            is_streaming: false,
            is_compacting: false,
            steering_mode: steering_mode.clone(),
            follow_up_mode: follow_up_mode.clone(),
            session_root,
            session_path,
            session_id,
            session_name,
            session_persistence,
            auto_compaction_enabled,
            auto_retry_enabled,
            messages: Vec::new(),
            repo,
            session,
            pending_bash_messages: Vec::new(),
            pending_session_events: Vec::new(),
            bash_abort_flags: Vec::new(),
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
            builtin_tools_enabled: !args.no_builtin_tools,
            prompt_templates,
            skills,
            active_tool_names: None,
            loaded_extensions,
            ui_request_sink: None,
        };
        let initial_tools = runtime.all_tools();
        runtime.active_tool_names = Some(rpc_select_active_tool_names(
            &runtime.extension_args,
            &runtime.settings,
            &initial_tools,
        ));
        runtime.refresh_extension_catalog();
        runtime.loaded_extensions.host.set_model(
            serde_json::to_value(&runtime.model)
                .ok()
                .filter(|value| !value.is_null()),
        );
        runtime
            .dispatch_pending_extension_actions()
            .await
            .map_err(|error| format!("startup extension action failed: {error}"))?;
        runtime.messages = runtime.load_context_messages().await?;
        Ok(runtime)
    }

    fn base_tools(&self) -> Vec<pi_agent::tools::AgentTool> {
        let mut tools = Vec::new();
        if self.tools_enabled && self.builtin_tools_enabled {
            tools.push(pi_agent::tools::bash_tool(self.cwd.clone()));
            tools.push(pi_agent::tools::read_tool_with_options(
                self.cwd.clone(),
                pi_agent::tools::image::ProcessImageOptions {
                    auto_resize_images: self.settings.get_image_auto_resize(),
                    ..Default::default()
                },
            ));
            tools.push(pi_agent::tools::write_tool(self.cwd.clone()));
            tools.push(pi_agent::tools::edit_tool(self.cwd.clone()));
            tools.push(crate::core::tools::ls_tool(self.cwd.clone()));
            tools.push(crate::core::tools::find_tool(self.cwd.clone()));
            tools.push(crate::core::tools::grep_tool(self.cwd.clone()));
        }
        tools
    }

    /// Publish the exact tool policy before the first prompt as well as when
    /// each prompt context is built.
    fn all_tools(&self) -> Vec<pi_agent::tools::AgentTool> {
        let mut tools = self.base_tools();
        install_tools(&self.loaded_extensions, &mut tools, self.tools_enabled);
        tools
    }

    fn active_tools_for_turn(&self) -> Vec<pi_agent::tools::AgentTool> {
        let mut tools = self.all_tools();
        if let Some(active_names) = &self.active_tool_names {
            tools.retain(|tool| active_names.iter().any(|name| name == &tool.tool.name));
        }
        tools
    }

    fn refresh_extension_catalog(&mut self) {
        let all_tools = self.all_tools();
        let available_names = all_tools
            .iter()
            .map(|tool| tool.tool.name.clone())
            .collect::<Vec<_>>();
        let active_names = self.active_tool_names.clone().map_or_else(
            || rpc_select_active_tool_names(&self.extension_args, &self.settings, &all_tools),
            |requested| {
                requested
                    .into_iter()
                    .filter(|name| available_names.iter().any(|available| available == name))
                    .collect()
            },
        );
        self.active_tool_names = Some(active_names.clone());
        let all_tool_values = all_tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.tool.name,
                    "description": tool.tool.description,
                    "parameters": tool.tool.parameters,
                })
            })
            .collect();
        let mut runner = self.loaded_extensions.runner.as_ref().clone();
        let commands = runner
            .get_registered_commands()
            .into_iter()
            .map(|command| {
                serde_json::json!({
                    "name": command.invocation_name,
                    "description": command.description,
                })
            })
            .collect();
        self.loaded_extensions
            .host
            .set_catalog(active_names, all_tool_values, commands);
    }

    fn rpc_commands(&mut self) -> Vec<serde_json::Value> {
        let mut commands = Vec::new();
        let mut runner = self.loaded_extensions.runner.as_ref().clone();
        for command in runner.get_registered_commands() {
            commands.push(rpc_slash_command_json(
                command.invocation_name,
                command.description,
                "extension",
                &command.source_info,
            ));
        }
        for template in &self.prompt_templates {
            commands.push(rpc_slash_command_json(
                template.name.clone(),
                Some(template.description.clone()),
                "prompt",
                &template.source_info,
            ));
        }
        for skill in &self.skills {
            commands.push(rpc_slash_command_json(
                format!("skill:{}", skill.name),
                Some(skill.description.clone()),
                "skill",
                &skill.source_info,
            ));
        }
        commands
    }

    fn extension_command_parts(message: &str) -> Option<(&str, &str)> {
        let text = message.strip_prefix('/')?;
        let (name, args) = text.find(char::is_whitespace).map_or((text, ""), |index| {
            (&text[..index], text[index..].trim_start())
        });
        (!name.is_empty()).then_some((name, args))
    }

    fn is_extension_command(&self, message: &str) -> bool {
        let Some((name, _)) = Self::extension_command_parts(message) else {
            return false;
        };
        let mut runner = self.loaded_extensions.runner.as_ref().clone();
        runner.get_command(name).is_some()
    }

    fn execute_extension_command(&self, message: &str) -> Option<Result<(), String>> {
        let (name, args) = Self::extension_command_parts(message)?;
        if !self.is_extension_command(message) {
            return None;
        }
        Some(
            self.loaded_extensions
                .runner
                .execute_command(name, args)
                .map(|_| ()),
        )
    }

    fn expand_rpc_prompt(&self, message: &str) -> String {
        let mut expanded = message.to_string();
        if let Some(stripped) = expanded.strip_prefix("/skill:") {
            let (name, args) = stripped
                .find(char::is_whitespace)
                .map_or((stripped, ""), |index| {
                    (&stripped[..index], stripped[index..].trim())
                });
            if let Some(skill) = self.skills.iter().find(|skill| skill.name == name) {
                if let Ok(raw) = std::fs::read_to_string(&skill.file_path) {
                    let body = pi_agent::harness::frontmatter::parse_frontmatter(&raw)
                        .map(|(_, body)| body)
                        .unwrap_or(raw)
                        .trim()
                        .to_string();
                    let block = format!(
                        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                        skill.name, skill.file_path, skill.base_dir, body
                    );
                    expanded = if args.is_empty() {
                        block
                    } else {
                        format!("{block}\n\n{args}")
                    };
                }
            }
        }
        crate::core::prompt_templates::expand_prompt_template(&expanded, &self.prompt_templates)
    }

    fn enqueue_rpc_message(&self, lane: &str, message: &str, images: &[ContentBlock]) {
        let queued = rpc_user_message(&self.expand_rpc_prompt(message), images);
        match lane {
            "steer" => self
                .steering_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .enqueue(queued),
            "follow_up" => self
                .follow_up_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .enqueue(queued),
            _ => unreachable!("invalid RPC queue lane: {lane}"),
        }
    }

    async fn apply_model_state(&mut self, model: Model, persist: bool) -> Result<(), String> {
        let previous_thinking = self.thinking_level;
        self.provider = model.provider.clone();
        self.model = model.clone();
        self.thinking_level = configured_thinking_level(&self.settings, &model);
        self.loaded_extensions.host.set_model(
            serde_json::to_value(&self.model)
                .ok()
                .filter(|value| !value.is_null()),
        );
        self.loaded_extensions
            .host
            .set_thinking_level(self.thinking_level.as_str());
        if persist {
            self.session
                .append_entry(
                    EntryNoStats::ModelChange {
                        id: format!("model-{}", pi_agent::session::new_id()),
                        provider: self.model.provider.clone(),
                        model_id: self.model.id.clone(),
                    },
                    "main",
                )
                .await
                .map_err(|error| format!("append model change: {error}"))?;
            if self.thinking_level != previous_thinking {
                self.session
                    .append_entry(
                        EntryNoStats::ThinkingLevel {
                            id: format!("thinking-{}", pi_agent::session::new_id()),
                            thinking_level: self.thinking_level.as_str().to_string(),
                        },
                        "main",
                    )
                    .await
                    .map_err(|error| format!("append thinking level: {error}"))?;
            }
        }
        Ok(())
    }

    async fn apply_thinking_level(
        &mut self,
        requested: pi_ai::types::ModelThinkingLevel,
        persist: bool,
    ) -> Result<(), String> {
        let next = pi_ai::model::clamp_thinking_level(&self.model, requested);
        let previous = self.thinking_level;
        self.thinking_level = next;
        self.loaded_extensions
            .host
            .set_thinking_level(self.thinking_level.as_str());
        if persist && next != previous {
            self.session
                .append_entry(
                    EntryNoStats::ThinkingLevel {
                        id: format!("thinking-{}", pi_agent::session::new_id()),
                        thinking_level: next.as_str().to_string(),
                    },
                    "main",
                )
                .await
                .map_err(|error| format!("append thinking level: {error}"))?;
        }
        Ok(())
    }

    async fn apply_active_tools(
        &mut self,
        requested: Vec<String>,
        persist: bool,
    ) -> Result<(), String> {
        let available = self
            .all_tools()
            .into_iter()
            .map(|tool| tool.tool.name)
            .collect::<HashSet<_>>();
        let mut active = Vec::new();
        for name in requested {
            if available.contains(&name) && !active.contains(&name) {
                active.push(name);
            }
        }
        self.active_tool_names = Some(active.clone());
        self.refresh_extension_catalog();
        if persist {
            self.session
                .append_entry(
                    EntryNoStats::ActiveTools {
                        id: format!("tools-{}", pi_agent::session::new_id()),
                        active_tool_names: active,
                    },
                    "main",
                )
                .await
                .map_err(|error| format!("append active tools: {error}"))?;
        }
        Ok(())
    }

    async fn apply_requested_extension_changes(&mut self) -> Result<(), String> {
        let changes = self.loaded_extensions.host.drain_requested_changes();
        if let Some(model_value) = changes.model {
            let provider = model_value
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "extension model request is missing provider".to_string())?;
            let model_id = model_value
                .get("id")
                .or_else(|| model_value.get("modelId"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "extension model request is missing id".to_string())?;
            let model = self.models.get_model(provider, model_id).ok_or_else(|| {
                format!("extension requested unknown model: {provider}/{model_id}")
            })?;
            self.apply_model_state(model, true).await?;
        }
        if let Some(active_tools) = changes.active_tools {
            self.apply_active_tools(active_tools, true).await?;
        }
        Ok(())
    }

    /// Available models snapshot (all catalog models across providers).
    fn available_models(&self) -> Vec<Model> {
        self.models.get_models(None)
    }

    fn queued_texts(queue: &PendingMessageQueue) -> Vec<String> {
        queue
            .snapshot()
            .into_iter()
            .filter_map(|message| match message {
                pi_agent::types::AgentMessage::Core(Message::User(user)) => {
                    Some(pi_agent::agent::user_content_text(&user))
                }
                _ => None,
            })
            .collect()
    }

    fn push_queue_update(&self, store: &mut Vec<String>) {
        let steering = Self::queued_texts(
            &self
                .steering_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        let follow_up = Self::queued_texts(
            &self
                .follow_up_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        store.push(serialize_json_line(&serde_json::json!({
            "type": "queue_update",
            "steering": steering,
            "followUp": follow_up,
        })));
    }

    fn push_compaction_start(store: &mut Vec<String>) {
        store.push(serialize_json_line(&serde_json::json!({
            "type": "compaction_start",
            "reason": "manual",
        })));
    }

    fn push_compaction_end(
        store: &mut Vec<String>,
        result: Option<&serde_json::Value>,
        error_message: Option<String>,
    ) {
        let mut event = serde_json::json!({
            "type": "compaction_end",
            "reason": "manual",
            "result": result.cloned().unwrap_or(serde_json::Value::Null),
            "aborted": false,
            "willRetry": false,
        });
        if let Some(error_message) = error_message {
            event["errorMessage"] = serde_json::Value::String(error_message);
        }
        store.push(serialize_json_line(&event));
    }

    fn runtime_simple_stream_options(&self) -> pi_ai::types::SimpleStreamOptions {
        let (provider_timeout_ms, provider_max_retries, max_retry_delay_ms) =
            self.settings.get_provider_retry_settings();
        let http_idle_timeout_ms = self.settings.get_http_idle_timeout_ms().unwrap_or(300_000);
        // Upstream maps a zero HTTP idle timeout to a large SDK timeout rather
        // than passing zero, which most SDKs interpret as an immediate timeout.
        let effective_idle_timeout_ms = if http_idle_timeout_ms == 0 {
            i32::MAX as u64
        } else {
            http_idle_timeout_ms
        };
        let websocket_connect_timeout_ms = self
            .settings
            .get_websocket_connect_timeout_ms()
            .ok()
            .flatten();

        pi_ai::types::SimpleStreamOptions {
            base: pi_ai::types::StreamOptions {
                base: pi_ai::types::ProviderRequestOptions {
                    api_key: self.api_key.clone(),
                    timeout_ms: Some(provider_timeout_ms.unwrap_or(effective_idle_timeout_ms)),
                    max_retries: provider_max_retries
                        .map(|retries| u32::try_from(retries).unwrap_or(u32::MAX)),
                    max_retry_delay_ms: Some(max_retry_delay_ms),
                    ..Default::default()
                },
                transport: Some(self.settings.get_transport().to_string()),
                session_id: Some(self.session_id.clone()),
                websocket_connect_timeout_ms,
                ..Default::default()
            },
            reasoning: thinking_level_for_request(self.thinking_level),
            thinking_budgets: thinking_budgets(&self.settings),
            ..Default::default()
        }
    }

    fn prompt_preflight_error(&self) -> Option<String> {
        let has_configured_auth = self.models.check_auth(&self.provider).is_some();
        let oauth_capable = self
            .models
            .get_provider(&self.provider)
            .is_some_and(|provider| provider.auth.oauth.is_some());
        rpc_prompt_preflight_error(
            &self.provider,
            self.api_key.as_deref(),
            has_configured_auth,
            oauth_capable,
        )
    }

    fn compaction_settings(&self) -> pi_agent::harness::compaction::CompactionSettings {
        let (_, reserve_tokens, keep_recent_tokens) = self.settings.get_compaction_settings();
        pi_agent::harness::compaction::CompactionSettings {
            enabled: self.auto_compaction_enabled,
            reserve_tokens,
            keep_recent_tokens,
        }
    }

    fn retry_policy(&self) -> pi_ai::utils::retry::RetryPolicy {
        let (enabled, max_retries, base_delay_ms) = self.settings.get_retry_settings();
        pi_ai::utils::retry::RetryPolicy {
            enabled,
            max_retries: u32::try_from(max_retries).unwrap_or(u32::MAX),
            base_delay_ms,
        }
    }

    fn simple_models(&self) -> SimpleModels {
        let models = self.models.clone();
        let faux_summary_core = self.faux_summary_core.clone();
        let defaults = self.runtime_simple_stream_options();
        SimpleModels::new(move |model, context, request_options| {
            let models = models.clone();
            let faux_summary_core = faux_summary_core.clone();
            let model = model.clone();
            let context = context.clone();
            let mut request_options = request_options.clone();
            apply_provider_defaults(&defaults, &mut request_options);
            Box::pin(async move {
                if let Some(core) = faux_summary_core {
                    return core
                        .stream(&model, &context, Some(&request_options))
                        .for_each(|_| {})
                        .await;
                }
                models
                    .complete_simple(&model, &context, Some(&request_options))
                    .await
            })
        })
    }

    fn bash_message(
        command: &str,
        result: &serde_json::Value,
        exclude_from_context: Option<bool>,
    ) -> pi_agent::types::AgentMessage {
        pi_agent::types::AgentMessage::Custom(pi_agent::types::CustomAgentMessage::BashExecution {
            command: command.to_string(),
            output: result
                .get("output")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            exit_code: result.get("exitCode").and_then(serde_json::Value::as_i64),
            cancelled: result
                .get("cancelled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            truncated: result
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            full_output_path: result
                .get("fullOutputPath")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            timestamp: pi_ai::types::now_ms(),
            exclude_from_context,
        })
    }

    async fn record_bash_result(
        &mut self,
        command: &str,
        result: &serde_json::Value,
        exclude_from_context: Option<bool>,
    ) -> Result<(), String> {
        let message = Self::bash_message(command, result, exclude_from_context);
        if self.is_streaming {
            self.pending_bash_messages.push(message);
            return Ok(());
        }

        self.messages.push(message.clone());
        self.persist_messages(std::slice::from_ref(&message)).await
    }

    async fn flush_pending_bash_messages(&mut self) -> Result<(), String> {
        if self.pending_bash_messages.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_bash_messages);
        self.messages.extend(pending.iter().cloned());
        self.persist_messages(&pending).await
    }

    fn register_bash_abort(&mut self, abort: Arc<AtomicBool>) {
        self.bash_abort_flags.push(abort);
    }

    fn unregister_bash_abort(&mut self, abort: &Arc<AtomicBool>) {
        self.bash_abort_flags
            .retain(|candidate| !Arc::ptr_eq(candidate, abort));
    }

    fn abort_all_bash(&self) {
        self.abort_bash.store(true, Ordering::SeqCst);
        for abort in &self.bash_abort_flags {
            abort.store(true, Ordering::SeqCst);
        }
    }

    /// Apply the shutdown side effects performed by the upstream runtime's
    /// `session.dispose()`: abort the active agent/retry/batch work and wake
    /// any extension dialog that can no longer receive input.
    fn abort_in_flight_work(&self) {
        self.abort_signal.store(true, Ordering::SeqCst);
        self.abort_retry_signal.store(true, Ordering::SeqCst);
        self.abort_all_bash();
        self.loaded_extensions.host.cancel_ui_requests();
    }

    /// Start standalone RPC bash execution without blocking prompt events or
    /// input processing. The response and session record are emitted when the
    /// task completes, matching upstream's concurrently handled command.
    fn start_bash_task(
        &mut self,
        command: RpcCommand,
        task_events: &UnboundedSender<RpcTaskMessage>,
        store: &mut Vec<String>,
    ) -> bool {
        let id = command.id.clone();
        let Some(bash_command) = command.str_field("command") else {
            store.push(serialize_json_line(&failure(
                id.as_deref(),
                "bash",
                "missing command".to_string(),
            )));
            return false;
        };
        let exclude_from_context = command.bool_field("excludeFromContext");
        let abort = Arc::new(AtomicBool::new(false));
        self.register_bash_abort(abort.clone());
        let cwd = self.cwd.clone();
        let task_events = task_events.clone();
        let update_id = id.clone();
        tokio::spawn(async move {
            let result = run_bash_with_updates(
                &bash_command,
                &cwd,
                abort.clone(),
                Some(task_events.clone()),
                update_id,
            )
            .await;
            let _ = task_events.send(RpcTaskMessage::Bash(RpcBashTaskResult {
                id,
                command: bash_command,
                exclude_from_context,
                abort,
                result,
            }));
        });
        true
    }

    /// The stream function used by the agent loop (facade-backed dispatch;
    /// faux has its scripted path echoing the prompt).
    fn make_stream_fn(&self, reply: &str) -> crate::run::StreamFn {
        let models = self.models.clone();
        let provider = self.provider.clone();
        let stream_options = self.runtime_simple_stream_options();
        if provider == "faux" {
            let core = self.faux_core.clone().unwrap_or_else(|| {
                crate::core::model_runtime::register_faux_provider(
                    &models,
                    &pi_ai::providers::RegisterFauxProviderOptions::default(),
                )
            });
            let reply = if reply.is_empty() {
                "Hello from pi-rust".to_string()
            } else {
                reply.to_string()
            };
            // Factory steps let tests observe the context the model receives
            // (e.g. multi-turn history seeding). Keep enough steps for queued
            // steering/follow-up turns to remain deterministic.
            let make_response = |reply: String| {
                let factory: FauxResponseFactory = Box::new(
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
            let stream_models = models.clone();
            return Arc::new(move |model, ctx| {
                stream_models.stream_simple(model, ctx, Some(&stream_options))
            });
        }
        Arc::new(move |model, ctx| models.stream_simple(model, ctx, Some(&stream_options)))
    }

    fn prepare_prompt_run(&self, message: &str, images: &[ContentBlock]) -> RpcPromptRun {
        let expanded_message = self.expand_rpc_prompt(message);
        let active_tools = self.active_tools_for_turn();
        let runner = self.loaded_extensions.runner.clone();
        let system_prompt_options = self
            .loaded_extensions
            .host
            .snapshot()
            .get("systemPromptOptions")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"cwd": self.cwd}));
        let hook_images = (!images.is_empty()).then(|| {
            serde_json::to_value(images).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
        });
        let before_agent_start = runner.emit_before_agent_start(
            &expanded_message,
            hook_images,
            self.system_prompt.as_deref().unwrap_or_default(),
            &system_prompt_options,
        );
        let system_prompt = before_agent_start
            .as_ref()
            .and_then(|result| result.get("systemPrompt"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| self.system_prompt.clone());
        let mut prompts = before_agent_start
            .and_then(|result| result.get("messages").cloned())
            .and_then(|messages| messages.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(rpc_extension_agent_message)
            .collect::<Vec<_>>();
        prompts.push(rpc_user_message(&expanded_message, images));

        // Seed the model context with prior history. The current prompt is
        // passed separately in `prompts`, matching the synchronous RPC path.
        let mut context = AgentContext::new(system_prompt, active_tools);
        context.messages = self.messages.clone();

        // The rich loop owns the queue drain points. Control commands can
        // therefore enqueue messages while this run is detached from the
        // mutable runtime.
        let steering_queue = self.steering_queue.clone();
        let follow_up_queue = self.follow_up_queue.clone();
        let steering_hook: pi_agent::rich_agent::AsyncHook<(), Vec<pi_agent::types::AgentMessage>> =
            Arc::new(move |_| {
                let queue = steering_queue.clone();
                Box::pin(async move {
                    queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .drain()
                })
            });
        let follow_up_hook: pi_agent::rich_agent::AsyncHook<
            (),
            Vec<pi_agent::types::AgentMessage>,
        > = Arc::new(move |_| {
            let queue = follow_up_queue.clone();
            Box::pin(async move {
                queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .drain()
            })
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
            block_images: self.settings.get_block_images(),
            before_tool_call: runner
                .has_handlers("tool_call")
                .then(|| rpc_before_tool_call_hook(runner.clone())),
            after_tool_call: runner
                .has_handlers("tool_result")
                .then(|| rpc_after_tool_call_hook(runner)),
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
            provider_uses_oauth: self
                .models
                .get_provider(&self.provider)
                .is_some_and(|registered| registered.auth.oauth.is_some()),
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
        let images = match rpc_images(&command) {
            Ok(images) => images,
            Err(error) => {
                fail(store, &id, &cmd, error);
                return None;
            }
        };
        if let Some((extension_name, extension_args)) = Self::extension_command_parts(&message) {
            let mut runner = self.loaded_extensions.runner.as_ref().clone();
            if runner.get_command(extension_name).is_some() {
                let (events, receiver) = mpsc::unbounded_channel();
                let extension_name = extension_name.to_string();
                let extension_args = extension_args.to_string();
                let command_id = id.clone();
                let command_type = cmd.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        runner.execute_command_with_ui(&extension_name, &extension_args, true);
                    let _ = events.send(RpcPromptTaskMessage::ExtensionCommandFinished {
                        id: command_id,
                        command: command_type,
                        result,
                    });
                });
                return Some(receiver);
            }
        }
        {
            let mut lock = self
                .run_lock
                .lock()
                .unwrap_or_else(|error| error.into_inner());
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
        if let Some(error) = self.prompt_preflight_error() {
            self.is_streaming = false;
            *self
                .run_lock
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = false;
            fail(store, &id, &cmd, error);
            return None;
        }
        // Build the prompt context, including the before-agent-start hook,
        // before acknowledging the command.  This keeps the response a true
        // preflight acknowledgement rather than a promise made before the
        // session has accepted the turn.
        let run = self.prepare_prompt_run(&message, &images);
        respond(store, success(id.as_deref(), &cmd, None));

        let (events, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run_rpc_prompt(run, events));
        Some(receiver)
    }

    async fn settle_prompt_with_persistence(
        &mut self,
        new_messages: Vec<pi_agent::types::AgentMessage>,
        persisted_messages: Vec<PersistedRpcMessage>,
    ) -> Vec<String> {
        let mut store = Vec::new();
        self.messages.extend(new_messages.iter().cloned());
        // Rich events normally cover every live message. If the run was
        // aborted before the assistant stream emitted its message lifecycle,
        // fall back to the live result so that the terminal assistant record
        // is not silently lost.
        let messages_to_persist =
            if persisted_messages.is_empty() || persisted_messages.len() < new_messages.len() {
                plain_persisted_messages(new_messages)
            } else {
                persisted_messages
            };
        let _ = self
            .persist_messages_with_termination(&messages_to_persist)
            .await;

        self.is_streaming = false;
        {
            let mut lock = self
                .run_lock
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *lock = false;
        }
        let auto_compacted = self.maybe_auto_compact().await.unwrap_or(false);
        store.extend(std::mem::take(&mut self.pending_session_events));
        if auto_compacted {
            store.push(serialize_json_line(
                &serde_json::json!({"type": "compacted"}),
            ));
        }
        // Standalone RPC bash is allowed to run alongside the agent. Defer
        // its context/session message until the agent lifecycle is complete,
        // exactly as the upstream session does for pending bash messages.
        let _ = self.flush_pending_bash_messages().await;
        if let Err(error) = self.dispatch_pending_extension_actions().await {
            store.push(serialize_json_line(&serde_json::json!({
                "type": "extension_error",
                "event": "host_action",
                "error": error,
            })));
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
        let settings = self.compaction_settings();
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
        let first_kept_entry_id = preparation.retained_tail.first().and_then(|kept| {
            entries.iter().find_map(|entry| {
                entry
                    .as_message()
                    .filter(|message| *message == kept)
                    .map(|_| entry.id().to_string())
            })
        });
        self.pending_session_events.push(serialize_json_line(
            &serde_json::json!({"type": "compaction_start", "reason": "threshold"}),
        ));
        let runner = self.loaded_extensions.runner.clone();
        let before = rpc_session_before_compact(
            runner.as_ref(),
            &preparation,
            &entries,
            first_kept_entry_id.as_deref(),
            None,
            "threshold",
            false,
        );
        if before
            .as_ref()
            .and_then(|result| result.get("cancel"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            self.pending_session_events
                .push(serialize_json_line(&serde_json::json!({
                    "type": "compaction_end",
                    "reason": "threshold",
                    "result": null,
                    "aborted": true,
                    "willRetry": false,
                })));
            rpc_session_compact_failed(runner.as_ref(), "threshold", true, false, false);
            return Ok(false);
        }

        let extension_compaction =
            rpc_extension_compaction_result(before.as_ref(), &preparation, &entries);
        let (
            summary,
            first_kept_entry_id,
            tokens_before,
            retained_tail,
            details,
            usage,
            from_extension,
        ) = if let Some((
            summary,
            first_kept_entry_id,
            tokens_before,
            retained_tail,
            details,
            usage,
        )) = extension_compaction
        {
            (
                summary,
                Some(first_kept_entry_id),
                tokens_before,
                retained_tail,
                details,
                usage,
                true,
            )
        } else {
            let options = self.simple_models();
            let retry = self.retry_policy();
            let thinking_level = self.thinking_level.as_str().to_string();
            let retry_events = Arc::new(Mutex::new(Vec::new()));
            let retry_callbacks = compaction_retry_callbacks(retry_events.clone(), "threshold");
            let compact_result = pi_agent::harness::compaction::compact(
                &preparation,
                &options,
                &self.model,
                None,
                None,
                Some(&thinking_level),
                Some(&retry),
                Some(&retry_callbacks),
            )
            .await;
            append_recorded_events(&mut self.pending_session_events, &retry_events);
            let result = match compact_result {
                Ok(result) => result,
                Err(error) => {
                    let error_message = format!("Auto-compaction failed: {error}");
                    self.pending_session_events
                        .push(serialize_json_line(&serde_json::json!({
                            "type": "compaction_end",
                            "reason": "threshold",
                            "result": null,
                            "aborted": false,
                            "willRetry": false,
                            "errorMessage": error_message,
                        })));
                    rpc_session_compact_failed(runner.as_ref(), "threshold", false, false, false);
                    return Err(format!("auto-compact: {error}"));
                }
            };
            let details = result.details.as_ref().map(|details| {
                serde_json::json!({
                    "readFiles": details.read_files,
                    "modifiedFiles": details.modified_files,
                })
            });
            (
                result.summary,
                first_kept_entry_id,
                result.tokens_before,
                result.retained_tail,
                details,
                result.usage,
                false,
            )
        };

        let summary_msg = pi_agent::agent::user_text_prompt(
            format!("[Compaction summary]\n{summary}"),
            pi_ai::types::now_ms(),
        );
        let mut replaced = vec![summary_msg];
        replaced.extend(retained_tail.clone());
        self.messages = replaced;

        let compaction_entry = match self
            .session
            .append_entry(
                EntryNoStats::Compaction {
                    id: format!("c-{}", pi_agent::session::new_id()),
                    summary: summary.clone(),
                    retained_tail,
                    tokens_before,
                    details: details.clone(),
                    usage: usage.clone(),
                },
                "main",
            )
            .await
        {
            Ok(entry) => entry,
            Err(error) => {
                let error_message = format!("Auto-compaction failed: persist: {error}");
                self.pending_session_events
                    .push(serialize_json_line(&serde_json::json!({
                        "type": "compaction_end",
                        "reason": "threshold",
                        "result": null,
                        "aborted": false,
                        "willRetry": false,
                        "errorMessage": error_message,
                    })));
                rpc_session_compact_failed(
                    runner.as_ref(),
                    "threshold",
                    false,
                    from_extension,
                    false,
                );
                return Err(format!("auto-compact: persist: {error}"));
            }
        };
        rpc_session_compact(
            runner.as_ref(),
            &compaction_entry,
            from_extension,
            "threshold",
            false,
        );
        let response = serde_json::json!({
            "summary": summary,
            "firstKeptEntryId": first_kept_entry_id,
            "tokensBefore": tokens_before,
            "estimatedTokensAfter": pi_agent::harness::compaction::estimate_context_tokens(&self.messages).tokens,
            "usage": usage,
            "details": details,
        });
        self.pending_session_events
            .push(serialize_json_line(&serde_json::json!({
                "type": "compaction_end",
                "reason": "threshold",
                "result": response,
                "aborted": false,
                "willRetry": false,
            })));
        Ok(true)
    }

    async fn persist_messages(
        &mut self,
        new_messages: &[pi_agent::types::AgentMessage],
    ) -> Result<(), String> {
        let messages = new_messages
            .iter()
            .cloned()
            .map(|message| PersistedRpcMessage {
                message,
                terminate: false,
            })
            .collect::<Vec<_>>();
        self.persist_messages_with_termination(&messages).await
    }

    async fn persist_messages_with_termination(
        &mut self,
        messages: &[PersistedRpcMessage],
    ) -> Result<(), String> {
        for persisted in messages {
            self.session
                .append_entry(
                    EntryNoStats::Message {
                        id: format!("m-{}", pi_agent::session::new_id()),
                        message: persisted.message.clone(),
                        terminate: persisted.terminate.then_some(true),
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
            pending_message_count: self
                .steering_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len()
                + self
                    .follow_up_queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .len(),
        }
    }

    fn extension_lifecycle_error(
        event: &str,
        errors: Vec<crate::core::extensions::ExtensionError>,
    ) -> String {
        let details = errors
            .into_iter()
            .map(|error| {
                format!(
                    "{} [{}]: {}",
                    error.extension_path, error.event, error.error
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("extension {event} failed: {details}")
    }

    fn session_before_switch_allowed(
        &self,
        reason: &str,
        target_session_file: Option<&str>,
    ) -> Result<bool, String> {
        self.loaded_extensions
            .runner
            .emit_session_before_switch(reason, target_session_file)
            .map(|cancelled| !cancelled)
            .map_err(|errors| Self::extension_lifecycle_error("session_before_switch", errors))
    }

    fn complete_pending_lifecycle(
        &self,
        request: Option<&PendingHostAction>,
        result: serde_json::Value,
    ) -> Result<(), String> {
        let Some(request) = request else {
            return Ok(());
        };
        // Native callers use the same queued host state but have no external
        // continuation to resolve. The loader sink treats these as a no-op;
        // avoiding the call also keeps hand-built test hosts compatible.
        if request
            .continuation_metadata()
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Ok(());
        }
        self.loaded_extensions
            .host
            .complete_lifecycle_action(request.clone(), result)
    }

    fn install_replacement_extensions(
        &mut self,
        reason: &str,
        previous_session_file: Option<&str>,
        flag_values: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        let loaded = load_for_mode_with_reason_and_flags_and_previous(
            &self.extension_args,
            &self.settings,
            &self.cwd,
            &self.agent_dir,
            "rpc",
            true,
            self.session_name.clone(),
            self.thinking_level.as_str(),
            reason,
            Some(flag_values),
            previous_session_file,
        );
        for error in &loaded.errors {
            tracing::warn!(path = %error.path, error = %error.error, "failed to load RPC extension");
        }
        self.loaded_extensions = loaded;
        if let Some(sink) = self.ui_request_sink.clone() {
            self.loaded_extensions.host.set_ui_request_sink(sink);
            self.loaded_extensions.host.set_ui_enabled(true);
        }
        self.loaded_extensions.host.set_model(
            serde_json::to_value(&self.model)
                .ok()
                .filter(|value| !value.is_null()),
        );
        self.loaded_extensions
            .host
            .set_thinking_level(self.thinking_level.as_str());
        register_loaded_native_providers(&self.models, &self.loaded_extensions)
            .map_err(|error| format!("failed to register RPC native providers: {error}"))?;
        self.prompt_templates = load_rpc_prompt_templates(
            &self.extension_args,
            &self.cwd,
            &self.agent_dir,
            &self.settings,
            &self.loaded_extensions.resources,
        );
        self.skills = load_rpc_skills(
            &self.extension_args,
            &self.cwd,
            &self.agent_dir,
            &self.settings,
            &self.loaded_extensions.resources,
        );
        self.refresh_extension_catalog();
        Ok(())
    }

    async fn replace_extension_runtime_for_reload_with_request(
        &mut self,
        request: Option<&PendingHostAction>,
    ) -> Result<(), String> {
        let old = self.loaded_extensions.clone();
        let flags = old.runner.get_flag_values();
        self.complete_pending_lifecycle(
            request,
            serde_json::json!({
                "cancelled": false,
                "result": "reload-started",
                "context": {"mode": "rpc", "cwd": self.cwd, "hasUI": true},
                "snapshot": old.host.snapshot(),
            }),
        )?;
        if let Err(errors) = old.runner.emit_session_shutdown("reload") {
            return Err(Self::extension_lifecycle_error("session_shutdown", errors));
        }
        old.runner.invalidate(Some("rpc extension reload"));
        self.settings.reload().await;
        self.install_replacement_extensions("reload", None, flags)
    }

    async fn replace_session_with_extensions(
        &mut self,
        session: JsonlSession<pi_agent::fs::StdFileSystem>,
        reason: &str,
        previous_session_file: Option<String>,
        completion: Option<(&PendingHostAction, serde_json::Value)>,
    ) -> Result<(), String> {
        let old = self.loaded_extensions.clone();
        let flags = old.runner.get_flag_values();
        let target_metadata = session.get_metadata().await;
        let target_session_file =
            (!target_metadata.path.is_empty()).then(|| target_metadata.path.clone());
        let target_session_id = target_metadata.id;
        let target_session_name = session.get_name().await;
        let _ = old.host.dispatch(
            ExtensionHostAction::SetSessionName,
            &serde_json::json!({"name": target_session_name}),
        );
        if let Some((request, mut result)) = completion {
            if let Some(object) = result.as_object_mut() {
                object.insert("snapshot".to_string(), old.host.snapshot());
                if object.get("context").is_none() {
                    object.insert(
                        "context".to_string(),
                        serde_json::json!({
                            "mode": "rpc",
                            "cwd": self.cwd,
                            "hasUI": true,
                            "sessionFile": target_session_file,
                            "sessionId": target_session_id,
                        }),
                    );
                }
            }
            self.complete_pending_lifecycle(Some(request), result)?;
        }
        if let Err(errors) = old
            .runner
            .emit_session_shutdown_with_target(reason, target_session_file.as_deref())
        {
            return Err(Self::extension_lifecycle_error("session_shutdown", errors));
        }
        old.runner.invalidate(Some("rpc session replacement"));

        self.session = session;
        self.session_path = target_session_file;
        self.session_id = target_session_id;
        self.session_persistence = matches!(self.session.kind(), SessionStorageKind::Jsonl(_));
        self.session_name = self.session.get_name().await;
        self.messages = self.load_context_messages().await?;
        self.install_replacement_extensions(reason, previous_session_file.as_deref(), flags)
    }

    async fn new_session_action(
        &mut self,
        parent_session: Option<String>,
        request: Option<&PendingHostAction>,
    ) -> Result<serde_json::Value, String> {
        if !self.session_before_switch_allowed("new", None)? {
            return Ok(serde_json::json!({"cancelled": true}));
        }
        let previous_session_file = self.session_path.clone();
        let session = if self.session_persistence {
            self.repo
                .create(CreateOptions {
                    id: Some(pi_agent::session::new_id()),
                    cwd: self.cwd.clone(),
                    parent_session_id: parent_session,
                    metadata: None,
                    fork_options: ForkOptions::Tree,
                })
                .await
                .map_err(|error| format!("create session: {error}"))?
        } else {
            let mut metadata = in_memory_metadata(
                format!("rpc-{}", pi_agent::session::new_id()),
                parent_session,
            );
            metadata.cwd = self.cwd.clone();
            let storage = Arc::new(Mutex::new(InMemorySessionStorage::new(metadata)));
            JsonlSession::from_in_memory(storage)
        };
        let target_session_file = session.get_metadata().await.path;
        let target_session_id = session.get_metadata().await.id;
        self.replace_session_with_extensions(
            session,
            "new",
            previous_session_file,
            request.map(|request| {
                (
                    request,
                    serde_json::json!({
                        "cancelled": false,
                        "result": "new-session-created",
                        "sessionFile": target_session_file,
                        "sessionId": target_session_id,
                    }),
                )
            }),
        )
        .await?;
        Ok(serde_json::json!({"cancelled": false}))
    }

    async fn navigate_tree_action(
        &mut self,
        target_id: String,
        options: &serde_json::Value,
        request: Option<&PendingHostAction>,
    ) -> Result<serde_json::Value, String> {
        if self.is_streaming || self.is_compacting {
            return Err(
                "Wait for the current operation to finish before navigating the session tree."
                    .to_string(),
            );
        }
        let current_leaf = self
            .session
            .get_leaf_id()
            .await
            .map_err(|error| error.to_string())?;
        if current_leaf.as_deref() == Some(target_id.as_str()) {
            return Ok(serde_json::json!({"cancelled": false}));
        }
        if self.session.get_entry(&target_id).await.is_none() {
            return Err(format!("Entry {target_id} not found"));
        }

        let target_entry = self
            .session
            .get_entry(&target_id)
            .await
            .ok_or_else(|| format!("Entry {target_id} not found"))?;
        let target_is_user_message = matches!(
            &target_entry,
            pi_agent::session::types::Entry::Message {
                message: pi_agent::types::AgentMessage::Core(Message::User(_)),
                ..
            }
        );
        // Navigating to a user/custom entry positions the leaf at its parent,
        // matching SessionManager.navigateTree. The next prompt can then
        // replace/re-edit that user input instead of creating a child of it.
        let new_leaf_id = if target_is_user_message {
            target_entry.parent_id().map(ToOwned::to_owned)
        } else {
            Some(target_id.clone())
        };
        let summarize = options
            .get("summarize")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let collected = pi_agent::harness::compaction::collect_entries_for_branch_summary(
            &self.session,
            current_leaf.as_deref(),
            &target_id,
        )
        .await
        .map_err(|error| format!("collect branch summary: {error}"))?;
        let preparation = serde_json::json!({
            "targetId": target_id,
            "oldLeafId": current_leaf,
            "commonAncestorId": collected.common_ancestor_id,
            "entriesToSummarize": collected.entries,
            "userWantsSummary": summarize,
            "customInstructions": options.get("customInstructions"),
            "replaceInstructions": options.get("replaceInstructions"),
            "label": options.get("label"),
        });
        let mut custom_instructions = options
            .get("customInstructions")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let mut replace_instructions = options
            .get("replaceInstructions")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut label = options
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let mut summary_text = None;
        let mut summary_details = None;
        let mut summary_usage = None;
        let mut summary_from_extension = false;
        match self.loaded_extensions.runner.emit(
            "session_before_tree",
            &serde_json::json!({
                "type": "session_before_tree",
                "preparation": preparation,
            }),
        ) {
            Ok(Some(result)) if result.get("cancel") == Some(&serde_json::Value::Bool(true)) => {
                return Ok(serde_json::json!({"cancelled": true}));
            }
            Ok(Some(result)) => {
                if let Some(value) = result.get("customInstructions").and_then(|v| v.as_str()) {
                    custom_instructions = Some(value.to_string());
                }
                if let Some(value) = result
                    .get("replaceInstructions")
                    .and_then(serde_json::Value::as_bool)
                {
                    replace_instructions = value;
                }
                if let Some(value) = result.get("label").and_then(|v| v.as_str()) {
                    label = Some(value.to_string());
                }
                if summarize {
                    if let Some(value) = result.get("summary").and_then(|v| v.as_str()) {
                        summary_text = Some(value.to_string());
                        summary_details = result.get("details").cloned();
                        summary_usage = result
                            .get("usage")
                            .cloned()
                            .and_then(|value| serde_json::from_value(value).ok());
                        summary_from_extension = true;
                    }
                }
            }
            Ok(None) => {}
            Err(errors) => {
                return Err(Self::extension_lifecycle_error(
                    "session_before_tree",
                    errors,
                ));
            }
        }

        if summarize && summary_text.is_none() && !collected.entries.is_empty() {
            let (reserve_tokens, _) = self.settings.get_branch_summary_settings();
            let retry = self.retry_policy();
            let result = pi_agent::harness::compaction::generate_branch_summary(
                &collected.entries,
                &self.simple_models(),
                &self.model,
                &pi_agent::harness::compaction::GenerateBranchSummaryOptions {
                    signal: Some(&self.abort_signal),
                    custom_instructions: custom_instructions.as_deref(),
                    replace_instructions,
                    reserve_tokens: Some(reserve_tokens),
                    retry: Some(&retry),
                    callbacks: None,
                },
            )
            .await
            .map_err(|error| format!("navigate tree summarization: {error}"))?;
            summary_text = Some(result.summary);
            summary_usage = result.usage;
            summary_details = Some(serde_json::json!({
                "readFiles": result.read_files,
                "modifiedFiles": result.modified_files,
            }));
        }

        self.session
            .move_lane("main", new_leaf_id.as_deref())
            .await
            .map_err(|error| format!("navigate tree: {error}"))?;
        let summary_entry = if let Some(summary) = summary_text {
            let entry = self
                .session
                .append_entry(
                    EntryNoStats::BranchSummary {
                        id: format!("bs-{}", pi_agent::session::new_id()),
                        from_id: current_leaf.clone().unwrap_or_else(|| "root".to_string()),
                        summary,
                        details: summary_details,
                        usage: summary_usage,
                    },
                    "main",
                )
                .await
                .map_err(|error| format!("append branch summary: {error}"))?;
            if let Some(label) = label.as_deref() {
                self.session
                    .set_label(entry.id(), Some(label))
                    .await
                    .map_err(|error| format!("label branch summary: {error}"))?;
            }
            Some(entry)
        } else if let Some(label) = label.as_deref() {
            self.session
                .set_label(&target_id, Some(label))
                .await
                .map_err(|error| format!("label tree target: {error}"))?;
            None
        } else {
            None
        };
        self.messages = self.load_context_messages().await?;
        let _ = self.loaded_extensions.runner.emit(
            "session_tree",
            &serde_json::json!({
                "type": "session_tree",
                "newLeafId": self.session.get_leaf_id().await.ok().flatten(),
                "oldLeafId": current_leaf,
                "summaryEntry": summary_entry,
                "fromExtension": summary_from_extension,
            }),
        );
        self.session
            .get_leaf_id()
            .await
            .map_err(|error| error.to_string())?;
        self.complete_pending_lifecycle(
            request,
            serde_json::json!({
                "cancelled": false,
                "result": "tree-navigated",
                "targetId": target_id,
                "editorText": if target_is_user_message {
                    target_entry
                        .as_message()
                        .and_then(|message| match message {
                            pi_agent::types::AgentMessage::Core(Message::User(user)) => {
                                Some(pi_agent::agent::user_content_text(user))
                            }
                            _ => None,
                        })
                } else {
                    None
                },
                "summaryEntry": summary_entry,
                "snapshot": self.loaded_extensions.host.snapshot(),
            }),
        )?;
        Ok(serde_json::json!({"cancelled": false}))
    }

    async fn dispatch_extension_action(
        &mut self,
        action: serde_json::Value,
        request: Option<&PendingHostAction>,
    ) -> Result<serde_json::Value, String> {
        let action_type = action
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "pending extension action is missing type".to_string())?;
        match action_type {
            "new_session" => {
                let options = action.get("options").cloned().unwrap_or_default();
                let parent_session = options
                    .get("parentSession")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
                self.new_session_action(parent_session, request).await
            }
            "fork" => {
                let entry_id = action
                    .get("entryId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "extension fork action is missing entryId".to_string())?
                    .to_string();
                let options = action.get("options").cloned().unwrap_or_default();
                let position = match options.get("position").and_then(serde_json::Value::as_str) {
                    None | Some("before") => ForkPosition::Before,
                    Some("at") => ForkPosition::At,
                    Some(value) => return Err(format!("invalid fork position: {value}")),
                };
                let result = self.fork_session(entry_id, position, request).await?;
                Ok(serde_json::json!({
                    "text": result.selected_text,
                    "cancelled": result.cancelled,
                }))
            }
            "navigate_tree" => {
                let target_id = action
                    .get("targetId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "extension navigateTree action is missing targetId".to_string())?
                    .to_string();
                let options = action.get("options").cloned().unwrap_or_default();
                self.navigate_tree_action(target_id, &options, request)
                    .await
            }
            "switch_session" => {
                let session_path = action
                    .get("sessionPath")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        "extension switchSession action is missing sessionPath".to_string()
                    })?;
                self.load_session_result(session_path, request).await
            }
            "reload" => {
                self.replace_extension_runtime_for_reload_with_request(request)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            other => Err(format!("unsupported pending extension action: {other}")),
        }
    }

    /// Consume lifecycle actions only after their extension callback has
    /// returned. A session replacement invalidates the old host, so any other
    /// action that was queued by the stale callback is intentionally discarded;
    /// actions queued by the fresh runtime are drained on the next iteration.
    async fn dispatch_pending_extension_actions(
        &mut self,
    ) -> Result<Vec<serde_json::Value>, String> {
        let mut results = Vec::new();
        loop {
            self.apply_requested_extension_changes().await?;
            let actions = self
                .loaded_extensions
                .host
                .drain_pending_lifecycle_action_metadata();
            if actions.is_empty() {
                break;
            }
            for request in actions {
                let action = request.payload.clone();
                let replaces_runtime = matches!(
                    action.get("type").and_then(serde_json::Value::as_str),
                    Some("new_session" | "fork" | "switch_session" | "reload")
                );
                results.push(
                    self.dispatch_extension_action(action, Some(&request))
                        .await?,
                );
                if replaces_runtime {
                    // Remaining requests belong to the invalidated callback
                    // context. Do not replay them against the new session.
                    break;
                }
            }
        }
        Ok(results)
    }

    /// Execute a single parsed command; returns the JSON lines to write
    /// (responses + streamed events).
    pub async fn handle_command(
        &mut self,
        command: RpcCommand,
        store: &mut Vec<String>,
    ) -> Result<(), String> {
        let result = self.handle_command_inner(command, store).await;
        let action_result = self.dispatch_pending_extension_actions().await;
        match (result, action_result) {
            (Err(command_error), Err(action_error)) => Err(format!(
                "{command_error}; pending extension action failed: {action_error}"
            )),
            (Err(command_error), Ok(_)) => Err(command_error),
            (Ok(()), Ok(_)) => Ok(()),
            (Ok(()), Err(action_error)) => {
                // The external bridge has already received its callback
                // acknowledgement before the mode boundary can mutate the
                // session. Preserve that command's successful wire response,
                // but surface the real post-callback failure as an RPC event.
                store.push(serialize_json_line(&serde_json::json!({
                    "type": "extension_error",
                    "event": "host_action",
                    "error": action_error,
                })));
                Ok(())
            }
        }
    }

    async fn handle_command_inner(
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
                let images = match rpc_images(&command) {
                    Ok(images) => images,
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                };
                if let Some(result) = self.execute_extension_command(&message) {
                    match result {
                        Ok(()) => respond(store, success(id.as_deref(), &cmd, None)),
                        Err(error) => fail(store, &id, &cmd, error),
                    }
                    return Ok(());
                }
                {
                    let mut lock = self
                        .run_lock
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if *lock {
                        match command.str_field("streamingBehavior").as_deref() {
                            Some("steer") => {
                                self.enqueue_rpc_message("steer", &message, &images);
                                self.push_queue_update(store);
                                respond(store, success(id.as_deref(), &cmd, None));
                            }
                            Some("followUp") => {
                                self.enqueue_rpc_message("follow_up", &message, &images);
                                self.push_queue_update(store);
                                respond(store, success(id.as_deref(), &cmd, None));
                            }
                            Some(value) => fail(
                                store,
                                &id,
                                &cmd,
                                format!("Invalid streaming behavior: {value}"),
                            ),
                            None => fail(
                                store,
                                &id,
                                &cmd,
                                "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.".to_string(),
                            ),
                        }
                        return Ok(());
                    }
                    *lock = true;
                }
                if let Some(error) = self.prompt_preflight_error() {
                    *self
                        .run_lock
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = false;
                    fail(store, &id, &cmd, error);
                    return Ok(());
                }
                self.is_streaming = true;
                self.abort_signal.store(false, Ordering::SeqCst);
                self.abort_retry_signal.store(false, Ordering::SeqCst);
                let RpcPromptRun {
                    prompts,
                    mut context,
                    config,
                    provider_uses_oauth,
                } = self.prepare_prompt_run(&message, &images);
                // Preflight success response is emitted before stream events,
                // after model/auth and before-agent-start preparation.
                respond(store, success(id.as_deref(), &cmd, None));

                let provider = config.model.provider.clone();
                let persisted_messages = Arc::new(Mutex::new(Vec::new()));
                let persisted_for_loop = persisted_messages.clone();
                let telemetry = HarnessTelemetryContext::default();
                let mut lifecycle = HarnessEventBus::new();
                let session_id = config
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "rpc-mode".to_string());
                let (new_messages, captured_events) = run_with_harness_lifecycle(
                    &telemetry,
                    &mut lifecycle,
                    "main",
                    &session_id,
                    String::new(),
                    move |_span| async move {
                        let mut captured_events = Vec::new();
                        let mut pending_terminations = HashSet::new();
                        let new_messages =
                            run_rich_agent_loop(prompts, &mut context, &config, &mut |event| {
                                capture_persisted_rpc_event(
                                    &event,
                                    &mut pending_terminations,
                                    &mut persisted_for_loop
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner()),
                                );
                                if let Some(line) = serialize_rpc_prompt_event_with_auth(
                                    event,
                                    Some(&provider),
                                    provider_uses_oauth,
                                ) {
                                    captured_events.push(line);
                                }
                            })
                            .await;
                        Ok((new_messages, captured_events))
                    },
                )
                .await
                .map_err(|error| error.to_string())?;

                // Emit the captured stream events in wire order.
                for line in captured_events {
                    store.push(line);
                }

                let persisted_messages = std::mem::take(
                    &mut *persisted_messages
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                );
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
                if self.is_extension_command(&message) {
                    fail(
                        store,
                        &id,
                        &cmd,
                        format!("Cannot queue extension command: {message}"),
                    );
                    return Ok(());
                }
                let images = match rpc_images(&command) {
                    Ok(images) => images,
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                };
                self.enqueue_rpc_message(&command.type_, &message, &images);
                self.push_queue_update(store);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "abort" => {
                self.abort_signal.store(true, Ordering::SeqCst);
                self.abort_retry_signal.store(true, Ordering::SeqCst);
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            "new_session" => {
                match self
                    .new_session_action(command.str_field("parentSession"), None)
                    .await
                {
                    Ok(result) => respond(store, success(id.as_deref(), &cmd, Some(result))),
                    Err(error) => fail(store, &id, &cmd, error),
                }
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
                let Some(provider_name) = command.str_field("provider") else {
                    fail(store, &id, &cmd, "missing provider".to_string());
                    return Ok(());
                };
                let Some(model_id) = command.str_field("modelId") else {
                    fail(store, &id, &cmd, "missing modelId".to_string());
                    return Ok(());
                };
                let model = self.models.get_model(&provider_name, &model_id);
                match model {
                    Some(model) => {
                        let model = model.clone();
                        let result = self.apply_model_state(model.clone(), true).await;
                        match result {
                            Ok(()) => respond(
                                store,
                                success(
                                    id.as_deref(),
                                    &cmd,
                                    Some(
                                        serde_json::to_value(model)
                                            .unwrap_or(serde_json::Value::Null),
                                    ),
                                ),
                            ),
                            Err(error) => fail(store, &id, &cmd, error),
                        }
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
                        let thinking_level = configured_thinking_level(&self.settings, &next);
                        match self.apply_model_state(next.clone(), true).await {
                            Ok(()) => {
                                let data = serde_json::json!({
                                    "model": next,
                                    "thinkingLevel": thinking_level.as_str(),
                                    "isScoped": false,
                                });
                                respond(store, success(id.as_deref(), &cmd, Some(data)));
                            }
                            Err(error) => fail(store, &id, &cmd, error),
                        }
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
                let previous = self.thinking_level;
                match self.apply_thinking_level(parsed, true).await {
                    Ok(()) => {}
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                }
                if self.thinking_level != previous {
                    store.push(serialize_json_line(&serde_json::json!({
                        "type": "thinking_level_changed",
                        "level": self.thinking_level.as_str(),
                    })));
                }
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
                let previous = self.thinking_level;
                if let Err(error) = self.apply_thinking_level(available[next_idx], true).await {
                    fail(store, &id, &cmd, error);
                    return Ok(());
                }
                if self.thinking_level != previous {
                    store.push(serialize_json_line(&serde_json::json!({
                        "type": "thinking_level_changed",
                        "level": self.thinking_level.as_str(),
                    })));
                }
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
                self.steering_queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .mode = if mode == "all" {
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
                self.follow_up_queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .mode = if mode == "all" {
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
                    &self.compaction_settings(),
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
                        Self::push_compaction_start(store);
                        let first_kept_entry_id =
                            preparation.retained_tail.first().and_then(|kept| {
                                entries.iter().find_map(|entry| {
                                    entry
                                        .as_message()
                                        .filter(|message| *message == kept)
                                        .map(|_| entry.id().to_string())
                                })
                            });
                        let runner = self.loaded_extensions.runner.clone();
                        let custom_instructions = command.str_field("customInstructions");
                        let before = rpc_session_before_compact(
                            runner.as_ref(),
                            &preparation,
                            &entries,
                            first_kept_entry_id.as_deref(),
                            custom_instructions.as_deref(),
                            "manual",
                            false,
                        );
                        if before
                            .as_ref()
                            .and_then(|result| result.get("cancel"))
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                        {
                            store.push(serialize_json_line(&serde_json::json!({
                                "type": "compaction_end",
                                "reason": "manual",
                                "result": null,
                                "aborted": true,
                                "willRetry": false,
                            })));
                            rpc_session_compact_failed(
                                runner.as_ref(),
                                "manual",
                                true,
                                false,
                                false,
                            );
                            fail(store, &id, &cmd, "Compaction cancelled".to_string());
                            return Ok(());
                        }
                        let extension_compaction = rpc_extension_compaction_result(
                            before.as_ref(),
                            &preparation,
                            &entries,
                        );
                        let (
                            summary,
                            first_kept_entry_id,
                            tokens_before,
                            retained_tail,
                            details,
                            usage,
                            from_extension,
                        ) = if let Some((
                            summary,
                            first_kept_entry_id,
                            tokens_before,
                            retained_tail,
                            details,
                            usage,
                        )) = extension_compaction
                        {
                            (
                                summary,
                                first_kept_entry_id,
                                tokens_before,
                                retained_tail,
                                details,
                                usage,
                                true,
                            )
                        } else {
                            let options = self.simple_models();
                            let model = self.model.clone();
                            let retry = self.retry_policy();
                            let thinking_level = self.thinking_level.as_str().to_string();
                            let retry_events = Arc::new(Mutex::new(Vec::new()));
                            let retry_callbacks =
                                compaction_retry_callbacks(retry_events.clone(), "manual");
                            let compact_result = pi_agent::harness::compaction::compact(
                                &preparation,
                                &options,
                                &model,
                                custom_instructions.as_deref(),
                                None,
                                Some(&thinking_level),
                                Some(&retry),
                                Some(&retry_callbacks),
                            )
                            .await;
                            append_recorded_events(store, &retry_events);
                            let result = match compact_result {
                                Ok(r) => r,
                                Err(e) => {
                                    let msg = format!("compact: {e}");
                                    Self::push_compaction_end(
                                        store,
                                        None,
                                        Some(format!("Compaction failed: {msg}")),
                                    );
                                    rpc_session_compact_failed(
                                        runner.as_ref(),
                                        "manual",
                                        false,
                                        false,
                                        false,
                                    );
                                    fail(store, &id, &cmd, msg);
                                    return Ok(());
                                }
                            };
                            let details = result.details.as_ref().map(|details| {
                                serde_json::json!({
                                    "readFiles": details.read_files,
                                    "modifiedFiles": details.modified_files,
                                })
                            });
                            let Some(first_kept_entry_id) = first_kept_entry_id else {
                                Self::push_compaction_end(
                                    store,
                                    None,
                                    Some(
                                        "Compaction failed: compact: first kept entry unavailable"
                                            .to_string(),
                                    ),
                                );
                                rpc_session_compact_failed(
                                    runner.as_ref(),
                                    "manual",
                                    false,
                                    false,
                                    false,
                                );
                                fail(
                                    store,
                                    &id,
                                    &cmd,
                                    "compact: first kept entry unavailable".to_string(),
                                );
                                return Ok(());
                            };
                            (
                                result.summary,
                                first_kept_entry_id,
                                result.tokens_before,
                                result.retained_tail,
                                details,
                                result.usage,
                                false,
                            )
                        };
                        self.messages = std::iter::once(pi_agent::agent::user_text_prompt(
                            format!("[Compaction summary]\n{summary}"),
                            pi_ai::types::now_ms(),
                        ))
                        .chain(retained_tail.iter().cloned())
                        .collect();
                        let compaction_entry = match self
                            .session
                            .append_entry(
                                EntryNoStats::Compaction {
                                    id: format!("c-{}", pi_agent::session::new_id()),
                                    summary: summary.clone(),
                                    retained_tail,
                                    tokens_before,
                                    details: details.clone(),
                                    usage: usage.clone(),
                                },
                                "main",
                            )
                            .await
                        {
                            Ok(entry) => entry,
                            Err(error) => {
                                let message = format!("compact: persist: {error}");
                                Self::push_compaction_end(
                                    store,
                                    None,
                                    Some(format!("Compaction failed: {message}")),
                                );
                                rpc_session_compact_failed(
                                    runner.as_ref(),
                                    "manual",
                                    false,
                                    from_extension,
                                    false,
                                );
                                fail(store, &id, &cmd, message);
                                return Ok(());
                            }
                        };
                        rpc_session_compact(
                            runner.as_ref(),
                            &compaction_entry,
                            from_extension,
                            "manual",
                            false,
                        );
                        let estimated_tokens_after =
                            pi_agent::harness::compaction::estimate_context_tokens(&self.messages)
                                .tokens;
                        let response = serde_json::json!({
                            "summary": summary,
                            "firstKeptEntryId": first_kept_entry_id,
                            "tokensBefore": tokens_before,
                            "estimatedTokensAfter": estimated_tokens_after,
                            "usage": usage,
                            "details": details,
                        });
                        Self::push_compaction_end(store, Some(&response), None);
                        response
                    }
                };
                respond(store, success(id.as_deref(), &cmd, Some(result)));
                Ok(())
            }

            // =================================================================
            // Bash
            // =================================================================
            "bash" => {
                let Some(bash_command) = command.str_field("command") else {
                    fail(store, &id, &cmd, "missing command".to_string());
                    return Ok(());
                };
                self.abort_bash.store(false, Ordering::SeqCst);
                let result = run_bash(&bash_command, &self.cwd, self.abort_bash.clone()).await;
                if let Err(error) = self
                    .record_bash_result(
                        &bash_command,
                        &result,
                        command.bool_field("excludeFromContext"),
                    )
                    .await
                {
                    fail(store, &id, &cmd, error);
                    return Ok(());
                }
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
                self.abort_all_bash();
                respond(store, success(id.as_deref(), &cmd, None));
                Ok(())
            }

            // =================================================================
            // Session
            // =================================================================
            "get_session_stats" => {
                let stats = match self.get_entries().await {
                    Ok(stats) => stats,
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                };
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
                let Some(session_path) = command.str_field("sessionPath") else {
                    fail(store, &id, &cmd, "missing sessionPath".to_string());
                    return Ok(());
                };
                match self.load_session_result(&session_path, None).await {
                    Ok(result) => {
                        respond(store, success(id.as_deref(), &cmd, Some(result)));
                        Ok(())
                    }
                    Err(e) => {
                        fail(store, &id, &cmd, e);
                        Ok(())
                    }
                }
            }

            "fork" => {
                let Some(entry_id) = command.str_field("entryId") else {
                    fail(store, &id, &cmd, "missing entryId".to_string());
                    return Ok(());
                };
                match self
                    .fork_session(entry_id, ForkPosition::Before, None)
                    .await
                {
                    Ok(result) => {
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(serde_json::json!({
                                    "text": result.selected_text.unwrap_or_default(),
                                    "cancelled": result.cancelled
                                })),
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

            "clone" => {
                let Some(entry_id) = self.session.get_leaf_id().await.ok().flatten() else {
                    fail(
                        store,
                        &id,
                        &cmd,
                        "Cannot clone session: no current entry selected".to_string(),
                    );
                    return Ok(());
                };
                match self.fork_session(entry_id, ForkPosition::At, None).await {
                    Ok(result) => {
                        respond(
                            store,
                            success(
                                id.as_deref(),
                                &cmd,
                                Some(serde_json::json!({"cancelled": result.cancelled})),
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

            "get_fork_messages" => {
                let entries = match self.get_entries().await {
                    Ok(entries) => entries,
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                };
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
                let mut entries = match self.get_entries().await {
                    Ok(entries) => entries,
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                };
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
                let entries = match self.get_entries().await {
                    Ok(entries) => entries,
                    Err(error) => {
                        fail(store, &id, &cmd, error);
                        return Ok(());
                    }
                };
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
                if let Err(error) = self.session.set_name(Some(&name)).await {
                    fail(store, &id, &cmd, error.to_string());
                    return Ok(());
                }
                self.session_name = Some(name.clone());
                store.push(serialize_json_line(&serde_json::json!({
                    "type": "session_info_changed",
                    "name": name,
                })));
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
                        Some(serde_json::json!({
                            "commands": self.rpc_commands(),
                        })),
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

    #[cfg(test)]
    #[cfg(test)]
    async fn load_session(&mut self, path: &str) -> Result<(), String> {
        self.load_session_result(path, None).await.map(|_| ())
    }

    async fn load_session_result(
        &mut self,
        path: &str,
        request: Option<&PendingHostAction>,
    ) -> Result<serde_json::Value, String> {
        // Load directly from the supplied path rather than looking it up in
        // the current session root: RPC clients may switch to a session from
        // another cwd/session directory.
        if !self.session_before_switch_allowed("resume", Some(path))? {
            return Ok(serde_json::json!({"cancelled": true}));
        }
        crate::core::session_migration::migrate_legacy_session_file(std::path::Path::new(path))
            .map_err(|e| format!("migrate legacy session {path:?}: {e}"))?;
        let storage = pi_agent::session::JsonlSessionStorage::load(
            pi_agent::fs::StdFileSystem::new(&self.cwd),
            path,
        )
        .await
        .map_err(|e| format!("failed to open session {path:?}: {e}"))?;
        let session = JsonlSession::new(storage);
        let previous_session_file = self.session_path.clone();
        let target_session_file = session.get_metadata().await.path;
        let target_session_id = session.get_metadata().await.id;
        self.replace_session_with_extensions(
            session,
            "resume",
            previous_session_file,
            request.map(|request| {
                (
                    request,
                    serde_json::json!({
                        "cancelled": false,
                        "result": "session-switched",
                        "sessionFile": target_session_file,
                        "sessionId": target_session_id,
                    }),
                )
            }),
        )
        .await?;
        Ok(serde_json::json!({"cancelled": false}))
    }

    fn session_fork_allowed(&self, entry_id: &str, position: ForkPosition) -> Result<bool, String> {
        let position_name = match position {
            ForkPosition::Before => "before",
            ForkPosition::At => "at",
        };
        match self
            .loaded_extensions
            .runner
            .emit_session_before_fork(entry_id, position_name)
        {
            Ok(cancelled) => Ok(!cancelled),
            Err(errors) => Err(Self::extension_lifecycle_error(
                "session_before_fork",
                errors,
            )),
        }
    }

    async fn fork_session(
        &mut self,
        entry_id: String,
        position: ForkPosition,
        request: Option<&PendingHostAction>,
    ) -> Result<ForkSessionResult, String> {
        // Upstream emits this before entry lookup. This preserves the
        // cancellation contract even for a stale/missing entry id.
        if !self.session_fork_allowed(&entry_id, position)? {
            return Ok(ForkSessionResult {
                selected_text: None,
                cancelled: true,
            });
        }

        let selected_text = if position == ForkPosition::Before {
            let entries = self.get_entries().await?;
            let entry = entries
                .iter()
                .find(|entry| entry.id() == entry_id)
                .ok_or_else(|| format!("Fork target not found: {entry_id}"))?;
            let Some(Message::User(user)) = entry.as_message().and_then(|message| match message {
                pi_agent::types::AgentMessage::Core(message) => Some(message),
                pi_agent::types::AgentMessage::Custom(_) => None,
            }) else {
                return Err(format!("Fork target is not a message entry: {entry_id}"));
            };
            Some(pi_agent::agent::user_content_text(user))
        } else {
            None
        };
        let metadata = self.session.get_metadata().await;
        let previous_session_file = self.session_path.clone();
        let fork_options = ForkOptions::Branch {
            entry_id: Some(entry_id),
            position: Some(position),
        };
        let session = if self.session_persistence {
            self.repo
                .fork(
                    &metadata,
                    CreateOptions {
                        id: Some(pi_agent::session::new_id()),
                        cwd: self.cwd.clone(),
                        parent_session_id: None,
                        metadata: None,
                        fork_options: fork_options.clone(),
                    },
                )
                .await
                .map_err(|e| format!("fork failed: {e}"))?
        } else {
            let mut forked_metadata = in_memory_metadata(
                format!("rpc-{}", pi_agent::session::new_id()),
                Some(metadata.id.clone()),
            );
            forked_metadata.cwd = self.cwd.clone();
            let forked_storage = match self.session.kind() {
                SessionStorageKind::InMemory(storage) => storage
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .fork(forked_metadata, &fork_options)
                    .map_err(|e| format!("fork failed: {e}"))?,
                SessionStorageKind::Jsonl(_) => {
                    return Err("in-memory fork requires an in-memory session".to_string());
                }
            };
            JsonlSession::from_in_memory(Arc::new(Mutex::new(forked_storage)))
        };
        let target_session_file = session.get_metadata().await.path;
        let target_session_id = session.get_metadata().await.id;
        self.replace_session_with_extensions(
            session,
            "fork",
            previous_session_file,
            request.map(|request| {
                (
                    request,
                    serde_json::json!({
                        "text": selected_text.clone(),
                        "cancelled": false,
                        "result": "session-forked",
                        "sessionFile": target_session_file,
                        "sessionId": target_session_id,
                    }),
                )
            }),
        )
        .await?;
        Ok(ForkSessionResult {
            selected_text,
            cancelled: false,
        })
    }
}

/// Run a bash command synchronously, capturing combined output
/// (upstream `BashResult` shape).
pub async fn run_bash(command: &str, cwd: &str, abort: Arc<AtomicBool>) -> serde_json::Value {
    run_bash_with_updates(command, cwd, abort, None, None).await
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
async fn run_bash_with_updates(
    command: &str,
    cwd: &str,
    abort: Arc<AtomicBool>,
    updates: Option<UnboundedSender<RpcTaskMessage>>,
    update_id: Option<String>,
) -> serde_json::Value {
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
            use tokio::io::AsyncReadExt;
            let mut stdout = child.stdout.take().expect("stdout piped");
            let mut stderr = child.stderr.take().expect("stderr piped");
            let mut stdout_done = false;
            let mut stderr_done = false;
            let mut stdout_buf = [0u8; 4096];
            let mut stderr_buf = [0u8; 4096];

            // Drain both pipes concurrently. The short polling branch makes
            // a silent command interruptible even though the cancellation
            // state is an atomic flag rather than an async signal.
            while !stdout_done || !stderr_done {
                if abort.load(Ordering::SeqCst) {
                    let _ = child.kill().await;
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                    result = stdout.read(&mut stdout_buf), if !stdout_done => {
                        match result {
                            Ok(0) => stdout_done = true,
                            Ok(n) => {
                                if let Some(updates) = &updates {
                                    let _ = updates.send(RpcTaskMessage::BashUpdate(RpcBashUpdate {
                                        id: update_id.clone(),
                                        delta: String::from_utf8_lossy(&stdout_buf[..n]).into_owned(),
                                    }));
                                }
                                if out.len() < BASH_TRUNCATE_LIMIT {
                                    let kept = n.min(BASH_TRUNCATE_LIMIT - out.len());
                                    out.extend_from_slice(&stdout_buf[..kept]);
                                    truncated |= kept < n || out.len() == BASH_TRUNCATE_LIMIT;
                                } else {
                                    truncated = true;
                                }
                            }
                            Err(_) => stdout_done = true,
                        }
                    }
                    result = stderr.read(&mut stderr_buf), if !stderr_done => {
                        match result {
                            Ok(0) => stderr_done = true,
                            Ok(n) => {
                                if let Some(updates) = &updates {
                                    let _ = updates.send(RpcTaskMessage::BashUpdate(RpcBashUpdate {
                                        id: update_id.clone(),
                                        delta: String::from_utf8_lossy(&stderr_buf[..n]).into_owned(),
                                    }));
                                }
                                if out.len() < BASH_TRUNCATE_LIMIT {
                                    let kept = n.min(BASH_TRUNCATE_LIMIT - out.len());
                                    out.extend_from_slice(&stderr_buf[..kept]);
                                    truncated |= kept < n || out.len() == BASH_TRUNCATE_LIMIT;
                                } else {
                                    truncated = true;
                                }
                            }
                            Err(_) => stderr_done = true,
                        }
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
        .map(serde_json::to_value)
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

fn parse_rpc_value(line: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(line).map_err(|e| format!("Failed to parse command: {e}"))
}

async fn next_rpc_shutdown_signal(receiver: &mut Option<UnboundedReceiver<i32>>) -> Option<i32> {
    match receiver.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
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
        Some(RpcPromptTaskMessage::ExtensionCommandFinished {
            id,
            command,
            result,
        }) => {
            let line = match result {
                Ok(_) => serialize_json_line(&success(id.as_deref(), &command, None)),
                Err(error) => serialize_json_line(&failure(id.as_deref(), &command, error)),
            };
            write_rpc_lines(out, std::iter::once(line)).await?;
            Ok(true)
        }
        None => Err("RPC prompt task ended without a completion message".to_string()),
    }
}

async fn handle_rpc_task_message<W: AsyncWrite + Unpin>(
    runtime: &mut RpcRuntime,
    message: Option<RpcTaskMessage>,
    out: &mut W,
    prompt_active: &mut bool,
    active_bashes: &mut usize,
    pending_abort_responses: &mut VecDeque<String>,
) -> Result<(), String> {
    match message {
        Some(RpcTaskMessage::Prompt(message)) => {
            if handle_rpc_prompt_task_message(runtime, Some(message), out, pending_abort_responses)
                .await?
            {
                *prompt_active = false;
            }
            Ok(())
        }
        Some(RpcTaskMessage::BashUpdate(update)) => {
            write_rpc_lines(
                out,
                std::iter::once(serialize_rpc_bash_update(
                    update.id.as_deref(),
                    &update.delta,
                )),
            )
            .await
        }
        Some(RpcTaskMessage::Bash(result)) => {
            *active_bashes = active_bashes.saturating_sub(1);
            runtime.unregister_bash_abort(&result.abort);
            if let Err(error) = runtime
                .record_bash_result(&result.command, &result.result, result.exclude_from_context)
                .await
            {
                return write_rpc_lines(
                    out,
                    std::iter::once(serialize_json_line(&failure(
                        result.id.as_deref(),
                        "bash",
                        error,
                    ))),
                )
                .await;
            }
            write_rpc_lines(
                out,
                std::iter::once(serialize_json_line(&success(
                    result.id.as_deref(),
                    "bash",
                    Some(result.result),
                ))),
            )
            .await
        }
        Some(RpcTaskMessage::ExtensionUiRequest(line)) => {
            write_rpc_lines(out, std::iter::once(line)).await
        }
        None => Err("RPC task channel ended unexpectedly".to_string()),
    }
}

async fn handle_rpc_runtime_command(
    runtime: &mut RpcRuntime,
    command: RpcCommand,
    store: &mut Vec<String>,
) {
    let id = command.id.clone();
    let command_name = command.type_.clone();
    if let Err(error) = runtime.handle_command(command, store).await {
        store.push(serialize_json_line(&failure(
            id.as_deref(),
            &command_name,
            error,
        )));
    }
}

fn start_forwarded_prompt_task(
    runtime: &mut RpcRuntime,
    command: RpcCommand,
    store: &mut Vec<String>,
    task_events: &UnboundedSender<RpcTaskMessage>,
    prompt_active: &mut bool,
) {
    let Some(receiver) = runtime.start_prompt_task(command, store) else {
        return;
    };
    *prompt_active = true;
    let task_events = task_events.clone();
    tokio::spawn(async move {
        let mut receiver = receiver;
        while let Some(message) = receiver.recv().await {
            if task_events.send(RpcTaskMessage::Prompt(message)).is_err() {
                break;
            }
        }
    });
}

struct RpcDispatchState<'a, W: AsyncWrite + Unpin> {
    out: &'a mut W,
    task_events: &'a UnboundedSender<RpcTaskMessage>,
    prompt_active: &'a mut bool,
    active_bashes: &'a mut usize,
    pending_commands: &'a mut VecDeque<RpcCommand>,
    pending_abort_responses: &'a mut VecDeque<String>,
}

async fn dispatch_rpc_command<W: AsyncWrite + Unpin>(
    runtime: &mut RpcRuntime,
    command: RpcCommand,
    state: &mut RpcDispatchState<'_, W>,
) -> Result<(), String> {
    // Standalone bash and abort_bash are always admitted, including while an
    // agent prompt is streaming. This is the key distinction from `abort`:
    // abort only targets the agent run, while abort_bash only targets shell
    // tasks.
    if command.type_ == "bash" {
        let mut store = Vec::new();
        if runtime.start_bash_task(command, state.task_events, &mut store) {
            *state.active_bashes += 1;
        }
        return write_rpc_lines(state.out, store).await;
    }
    if command.type_ == "abort_bash" {
        let mut store = Vec::new();
        handle_rpc_runtime_command(runtime, command, &mut store).await;
        return write_rpc_lines(state.out, store).await;
    }

    if *state.prompt_active {
        if can_handle_during_prompt(&command) {
            let is_abort = command.type_ == "abort";
            let mut store = Vec::new();
            handle_rpc_runtime_command(runtime, command, &mut store).await;
            if is_abort {
                state.pending_abort_responses.extend(store);
                return Ok(());
            }
            return write_rpc_lines(state.out, store).await;
        }
        state.pending_commands.push_back(command);
        return Ok(());
    }

    if command.type_ == "prompt" {
        let mut store = Vec::new();
        start_forwarded_prompt_task(
            runtime,
            command,
            &mut store,
            state.task_events,
            state.prompt_active,
        );
        return write_rpc_lines(state.out, store).await;
    }

    let mut store = Vec::new();
    handle_rpc_runtime_command(runtime, command, &mut store).await;
    write_rpc_lines(state.out, store).await
}

async fn dispatch_rpc_line<W: AsyncWrite + Unpin>(
    runtime: &mut RpcRuntime,
    line: String,
    state: &mut RpcDispatchState<'_, W>,
) -> Result<(), String> {
    if line.trim().is_empty() {
        return Ok(());
    }
    let parsed = match parse_rpc_value(&line) {
        Ok(parsed) => parsed,
        Err(error) => {
            let response = failure(None, "parse", error);
            return write_rpc_lines(state.out, std::iter::once(serialize_json_line(&response)))
                .await;
        }
    };
    if parsed.get("type").and_then(serde_json::Value::as_str) == Some("extension_ui_input") {
        let id = parsed.get("id").and_then(serde_json::Value::as_str);
        let data = parsed
            .get("data")
            .or_else(|| parsed.get("input"))
            .and_then(serde_json::Value::as_str);
        let Some(data) = data else {
            let response = failure(
                id,
                "extension_ui_input",
                "extension_ui_input requires a string data or input field".to_string(),
            );
            return write_rpc_lines(state.out, std::iter::once(serialize_json_line(&response)))
                .await;
        };
        let dispatch = match runtime.loaded_extensions.host.dispatch_terminal_input(data) {
            Ok(dispatch) => dispatch,
            Err(error) => {
                let response = failure(id, "extension_ui_input", error);
                return write_rpc_lines(state.out, std::iter::once(serialize_json_line(&response)))
                    .await;
            }
        };
        let mut response = dispatch.as_value();
        response["type"] = serde_json::Value::String("extension_ui_input_result".to_string());
        if let Some(id) = id {
            response["id"] = serde_json::Value::String(id.to_string());
        }
        return write_rpc_lines(state.out, std::iter::once(serialize_json_line(&response))).await;
    }
    if parsed.get("type").and_then(serde_json::Value::as_str) == Some("extension_ui_response") {
        let disposition = runtime.loaded_extensions.host.handle_ui_response(&parsed);
        if disposition == ExtensionUiResponseDisposition::Resolved {
            return Ok(());
        }
        let diagnostics = runtime.loaded_extensions.host.drain_ui_diagnostics();
        let lines = diagnostics.into_iter().map(|diagnostic| {
            let mut value = serde_json::json!({
                "type": "extension_ui_diagnostic",
                "code": diagnostic.code,
                "message": diagnostic.message,
            });
            if let Some(id) = diagnostic.id {
                value["id"] = serde_json::Value::String(id);
            }
            serialize_json_line(&value)
        });
        return write_rpc_lines(state.out, lines).await;
    }
    let command = match RpcCommand::parse(parsed) {
        Ok(command) => command,
        Err(error) => {
            let response = failure(None, "parse", error);
            return write_rpc_lines(state.out, std::iter::once(serialize_json_line(&response)))
                .await;
        }
    };
    dispatch_rpc_command(runtime, command, state).await
}

/// Run the RPC mode loop: read commands from stdin, write responses/events
/// to stdout until EOF.
pub async fn run_rpc_mode(args: &Args, settings: SettingsManager) -> Result<(), String> {
    let mut runtime = RpcRuntime::new(args, settings).await?;
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = JsonlLineReader::new(stdin);
    let mut out = tokio::io::BufWriter::new(stdout);
    let (task_events, mut task_receiver) = mpsc::unbounded_channel::<RpcTaskMessage>();
    let ui_task_events = task_events.clone();
    runtime.bind_ui_request_sink(Arc::new(move |request| {
        ui_task_events
            .send(RpcTaskMessage::ExtensionUiRequest(serialize_json_line(
                &request,
            )))
            .map_err(|_| "RPC UI request channel is closed".to_string())
    }));
    let mut prompt_active = false;
    let mut active_bashes = 0usize;
    let mut pending_commands = VecDeque::new();
    let mut pending_abort_responses = VecDeque::new();
    let mut input_closed = false;
    let mut shutdown_requested = None::<i32>;

    #[cfg(unix)]
    let mut shutdown_signal = {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate())
            .map_err(|error| format!("install SIGTERM handler: {error}"))?;
        let mut sighup = signal(SignalKind::hangup())
            .map_err(|error| format!("install SIGHUP handler: {error}"))?;
        let (sender, receiver) = mpsc::unbounded_channel::<i32>();
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => {
                    let _ = sender.send(143);
                }
                _ = sighup.recv() => {
                    let _ = sender.send(129);
                }
            }
        });
        Some(receiver)
    };
    #[cfg(not(unix))]
    let mut shutdown_signal = None::<UnboundedReceiver<i32>>;

    loop {
        if !prompt_active {
            if let Some(command) = pending_commands.pop_front() {
                let mut state = RpcDispatchState {
                    out: &mut out,
                    task_events: &task_events,
                    prompt_active: &mut prompt_active,
                    active_bashes: &mut active_bashes,
                    pending_commands: &mut pending_commands,
                    pending_abort_responses: &mut pending_abort_responses,
                };
                dispatch_rpc_command(&mut runtime, command, &mut state).await?;
                continue;
            }
        }

        if input_closed {
            if !prompt_active && active_bashes == 0 {
                break;
            }
            handle_rpc_task_message(
                &mut runtime,
                task_receiver.recv().await,
                &mut out,
                &mut prompt_active,
                &mut active_bashes,
                &mut pending_abort_responses,
            )
            .await?;
            continue;
        }

        if prompt_active || active_bashes > 0 {
            tokio::select! {
                biased;
                task = task_receiver.recv() => {
                    handle_rpc_task_message(
                        &mut runtime,
                        task,
                        &mut out,
                        &mut prompt_active,
                        &mut active_bashes,
                        &mut pending_abort_responses,
                    ).await?;
                    }
                    line = reader.next_line() => {
                        match line.map_err(|e| format!("stdin read error: {e}"))? {
                        Some(line) => {
                            let mut state = RpcDispatchState {
                                out: &mut out,
                                task_events: &task_events,
                                prompt_active: &mut prompt_active,
                                active_bashes: &mut active_bashes,
                                pending_commands: &mut pending_commands,
                                pending_abort_responses: &mut pending_abort_responses,
                            };
                            dispatch_rpc_line(&mut runtime, line, &mut state).await?
                        }
                        None => {
                            input_closed = true;
                            // EOF removes the only possible response source.
                            // Abort active work and wake a detached extension
                            // command immediately instead of waiting for a
                            // provider, shell, or dialog timeout.
                            runtime.abort_in_flight_work();
                        },
                    }
                }
                signal = next_rpc_shutdown_signal(&mut shutdown_signal), if shutdown_requested.is_none() => {
                    if let Some(code) = signal {
                        shutdown_requested = Some(code);
                        input_closed = true;
                        runtime.abort_in_flight_work();
                        shutdown_signal = None;
                    }
                }
            }
        } else {
            // UI actions can be emitted while the mode is idle (including
            // fire-and-forget lifecycle actions flushed when the sink binds),
            // so the task channel must remain live even without a prompt or
            // bash worker.
            tokio::select! {
                biased;
                task = task_receiver.recv() => {
                    handle_rpc_task_message(
                        &mut runtime,
                        task,
                        &mut out,
                        &mut prompt_active,
                        &mut active_bashes,
                        &mut pending_abort_responses,
                    ).await?;
                }
                line = reader.next_line() => {
                    match line.map_err(|e| format!("stdin read error: {e}"))? {
                    Some(line) => {
                        let mut state = RpcDispatchState {
                            out: &mut out,
                            task_events: &task_events,
                            prompt_active: &mut prompt_active,
                            active_bashes: &mut active_bashes,
                            pending_commands: &mut pending_commands,
                            pending_abort_responses: &mut pending_abort_responses,
                        };
                        dispatch_rpc_line(&mut runtime, line, &mut state).await?
                    }
                    None => {
                        input_closed = true;
                        runtime.abort_in_flight_work();
                    }
                    }
                }
                signal = next_rpc_shutdown_signal(&mut shutdown_signal), if shutdown_requested.is_none() => {
                    if let Some(code) = signal {
                        shutdown_requested = Some(code);
                        input_closed = true;
                        runtime.abort_in_flight_work();
                        shutdown_signal = None;
                    }
                }
            }
        }
    }

    if let Some(code) = shutdown_requested {
        if code == 129 {
            out.flush().await.map_err(|error| error.to_string())?;
        }
        drop(out);
        drop(runtime);
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use crate::core::extensions::ExtensionHostActions;
    use std::collections::BTreeMap;

    use super::*;

    async fn runtime_for_test() -> RpcRuntime {
        runtime_for_test_with_settings(SettingsManager::in_memory(Default::default())).await
    }

    fn next_test_ui_request(
        host: &crate::core::extensions::ExtensionHostState,
    ) -> serde_json::Value {
        for _ in 0..500 {
            if let Some(request) = host.drain_ui_requests().into_iter().next() {
                return request;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("RPC test UI request was not emitted");
    }

    async fn dispatch_test_ui_response(
        runtime: &mut RpcRuntime,
        response: serde_json::Value,
    ) -> Result<(), String> {
        let (task_events, _task_receiver) = mpsc::unbounded_channel::<RpcTaskMessage>();
        let mut output = tokio::io::sink();
        let mut prompt_active = false;
        let mut active_bashes = 0;
        let mut pending_commands = VecDeque::new();
        let mut pending_abort_responses = VecDeque::new();
        let mut state = RpcDispatchState {
            out: &mut output,
            task_events: &task_events,
            prompt_active: &mut prompt_active,
            active_bashes: &mut active_bashes,
            pending_commands: &mut pending_commands,
            pending_abort_responses: &mut pending_abort_responses,
        };
        dispatch_rpc_line(runtime, serialize_json_line(&response), &mut state).await
    }

    #[tokio::test]
    async fn rpc_fork_cancellation_happens_before_entry_validation() {
        let mut runtime = runtime_for_test().await;
        let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let events_for_handler = Arc::clone(&events);
        let handler = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, event: &serde_json::Value| {
                events_for_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(Some(serde_json::json!({"cancel": true})))
            },
        ) as crate::core::extensions::HandlerFn;
        let mut extension = crate::core::extensions::Extension {
            path: "rpc-fork-hook.js".to_string(),
            ..Default::default()
        };
        extension
            .handlers
            .insert("session_before_fork".to_string(), vec![handler]);
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        let runner = Arc::new(crate::core::extensions::ExtensionRunner::new(
            vec![extension],
            Arc::clone(&extension_runtime),
            runtime.cwd.clone(),
        ));
        runtime.loaded_extensions = LoadedExtensions {
            runner,
            host: Arc::new(crate::core::extensions::ExtensionHostState::new(
                None, "off",
            )),
            runtime: extension_runtime,
            errors: Vec::new(),
            resources: crate::core::extensions::ResourceDiscovery::default(),
        };

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "fork",
                    "type": "fork",
                    "entryId": "missing-entry"
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();

        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["cancelled"], true);
        assert_eq!(
            *events.lock().unwrap_or_else(|error| error.into_inner()),
            vec![serde_json::json!({
                "type": "session_before_fork",
                "entryId": "missing-entry",
                "position": "before"
            })]
        );
    }

    #[tokio::test]
    async fn rpc_fork_success_shuts_down_old_runner_before_rebinding() {
        let mut runtime = runtime_for_test().await;
        run_test_prompt(&mut runtime, "hello").await;
        let previous_session_file = runtime.session_path.clone().unwrap();
        let entry_id = runtime
            .get_entries()
            .await
            .unwrap()
            .iter()
            .find(|entry| entry.as_message().is_some())
            .map(|entry| entry.id().to_string())
            .unwrap();

        let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let events_for_handler = Arc::clone(&events);
        let handler = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, event: &serde_json::Value| {
                events_for_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event.clone());
                Ok(None)
            },
        ) as crate::core::extensions::HandlerFn;
        let mut extension = crate::core::extensions::Extension {
            path: "rpc-fork-order.js".to_string(),
            ..Default::default()
        };
        for event_type in ["session_before_fork", "session_shutdown", "session_start"] {
            extension
                .handlers
                .insert(event_type.to_string(), vec![Arc::clone(&handler)]);
        }
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        runtime.loaded_extensions = LoadedExtensions {
            runner: Arc::new(crate::core::extensions::ExtensionRunner::new(
                vec![extension],
                Arc::clone(&extension_runtime),
                runtime.cwd.clone(),
            )),
            host: Arc::new(crate::core::extensions::ExtensionHostState::new(
                None, "off",
            )),
            runtime: extension_runtime,
            errors: Vec::new(),
            resources: crate::core::extensions::ResourceDiscovery::default(),
        };
        let old_runner = runtime.loaded_extensions.runner.clone();

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "fork",
                    "type": "fork",
                    "entryId": entry_id
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();

        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["cancelled"], false);
        let target_session_file = runtime.session_path.clone().unwrap();
        let events = events.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(events[0]["type"], "session_before_fork");
        assert_eq!(events[0]["position"], "before");
        assert_eq!(events[1]["type"], "session_shutdown");
        assert_eq!(events[1]["reason"], "fork");
        assert_eq!(events[1]["targetSessionFile"], target_session_file);
        assert_eq!(events.len(), 2);
        assert!(!Arc::ptr_eq(&old_runner, &runtime.loaded_extensions.runner));
        assert_ne!(
            runtime.session_path.as_deref(),
            Some(previous_session_file.as_str())
        );
    }

    #[tokio::test]
    async fn rpc_dispatches_queued_new_session_action_and_rebinds_runtime() {
        let mut runtime = runtime_for_test().await;
        let previous_session_id = runtime.session_id.clone();
        let previous_session_path = runtime.session_path.clone();
        let old_runner = runtime.loaded_extensions.runner.clone();
        old_runner.set_flag_value("rpc-preserved-flag", serde_json::json!("kept"));

        runtime
            .loaded_extensions
            .host
            .dispatch(
                crate::core::extensions::ExtensionHostAction::NewSession,
                &serde_json::json!({"options": {}}),
            )
            .unwrap();

        let results = runtime.dispatch_pending_extension_actions().await.unwrap();

        assert_eq!(results, vec![serde_json::json!({"cancelled": false})]);
        assert_ne!(runtime.session_id, previous_session_id);
        assert_ne!(runtime.session_path, previous_session_path);
        assert!(!Arc::ptr_eq(&old_runner, &runtime.loaded_extensions.runner));
        assert_eq!(
            runtime
                .loaded_extensions
                .runner
                .get_flag_values()
                .get("rpc-preserved-flag"),
            Some(&serde_json::json!("kept"))
        );
        assert!(old_runner.emit_session_start("stale").is_err());
    }

    async fn runtime_for_test_with_settings(settings: SettingsManager) -> RpcRuntime {
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
            "--no-skills".to_string(),
            "--no-prompt-templates".to_string(),
        ])
        .expect_run();
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
    fn rpc_prompt_preflight_requires_auth_before_acknowledgement() {
        let missing_key = rpc_prompt_preflight_error("anthropic", None, false, false)
            .expect("missing API key must fail preflight");
        assert!(missing_key.starts_with("No API key found for anthropic."));
        assert!(missing_key.contains("Use /login"));
        assert!(
            rpc_prompt_preflight_error("anthropic", Some("runtime-key"), false, false).is_none()
        );
        assert!(rpc_prompt_preflight_error("anthropic", Some(""), false, false).is_some());
        assert!(rpc_prompt_preflight_error("anthropic", None, true, false).is_none());

        let oauth_missing = rpc_prompt_preflight_error("openai-codex", None, false, true)
            .expect("missing OAuth credential must fail preflight");
        assert_eq!(
            oauth_missing,
            "Authentication failed for \"openai-codex\". Credentials may have expired or network is unavailable. Run '/login openai-codex' to re-authenticate."
        );
    }

    #[test]
    fn rpc_provider_auth_errors_include_login_guidance() {
        let mut message = pi_ai::types::AssistantMessage::new();
        message.set_api_provider_model("google", "google", "gemini");
        message.set_stop_reason(pi_ai::types::StopReason::Error);
        message.set_error_message("Provider is not configured: google");
        let line = serialize_rpc_prompt_event_with_auth(
            RichAgentEvent::MessageUpdate {
                message: pi_agent::types::AgentMessage::Core(Message::Assistant(message.clone())),
                assistant_message_event: AssistantMessageEvent::Error {
                    reason: pi_ai::types::ErrorReason::Error,
                    error_message: message,
                },
            },
            Some("google"),
            false,
        )
        .expect("error event should serialize");
        let json: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let error = &json["assistantMessageEvent"]["error"]["errorMessage"];
        assert!(error
            .as_str()
            .unwrap()
            .starts_with("No API key found for google."));
        assert!(error.as_str().unwrap().contains("Use /login"));
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
        assert_eq!(json["type"], "message_end");
        assert_eq!(json["message"]["stopReason"], "error");
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
                plain_persisted_messages(vec![prompt, failed, recovered]),
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
            provider_uses_oauth: false,
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
                RpcPromptTaskMessage::ExtensionCommandFinished { .. } => {
                    panic!("prompt worker emitted an extension-command completion")
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
        assert!(values[..retry_start].iter().any(|value| {
            value["type"] == "message_end" && value["message"]["stopReason"] == "error"
        }));
        assert!(values[..retry_start]
            .iter()
            .any(|value| value["assistantMessageEvent"]["type"] == "text_delta"));
        assert!(values[retry_start..retry_end].iter().any(|value| {
            value["type"] == "message_end" && value["message"]["stopReason"] == "stop"
        }));
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
    async fn get_commands_discovers_extension_prompt_and_skill_sources() {
        let mut runtime = runtime_for_test().await;
        let extension_source = crate::core::extensions::SourceInfo {
            path: "/tmp/rpc-extension.rs".to_string(),
            source: "extension:rpc-extension".to_string(),
            scope: "temporary".to_string(),
            origin: "top-level".to_string(),
            base_dir: Some("/tmp".to_string()),
        };
        let seen_args = Arc::new(Mutex::new(Vec::new()));
        let seen_args_for_handler = Arc::clone(&seen_args);
        let handler = Arc::new(
            move |_: &crate::core::extensions::ExtensionContext, event: &serde_json::Value| {
                seen_args_for_handler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event["args"].as_str().unwrap_or_default().to_string());
                Ok(None)
            },
        ) as crate::core::extensions::HandlerFn;
        let mut extension = crate::core::extensions::Extension {
            path: extension_source.path.clone(),
            source_info: extension_source.clone(),
            ..Default::default()
        };
        extension.commands.insert(
            "hello".to_string(),
            crate::core::extensions::RegisteredCommand {
                name: "hello".to_string(),
                source_info: extension_source.clone(),
                description: Some("Say hello".to_string()),
                handler,
            },
        );
        let extension_runtime = Arc::new(Mutex::new(
            crate::core::extensions::types::ExtensionRuntime::new(),
        ));
        runtime.loaded_extensions = LoadedExtensions {
            runner: Arc::new(crate::core::extensions::ExtensionRunner::new(
                vec![extension],
                Arc::clone(&extension_runtime),
                runtime.cwd.clone(),
            )),
            host: Arc::new(crate::core::extensions::ExtensionHostState::new(
                None, "off",
            )),
            runtime: extension_runtime,
            errors: Vec::new(),
            resources: crate::core::extensions::ResourceDiscovery::default(),
        };
        runtime.prompt_templates = vec![crate::core::prompt_templates::PromptTemplate {
            name: "review".to_string(),
            description: "Review the change".to_string(),
            argument_hint: None,
            content: "Review $@".to_string(),
            source_info: crate::core::extensions::SourceInfo::synthetic(
                "/tmp/review.md",
                "local",
                Some("/tmp".to_string()),
            ),
            file_path: "/tmp/review.md".to_string(),
        }];
        runtime.skills = vec![crate::core::skills::Skill {
            name: "lint".to_string(),
            description: "Run lint checks".to_string(),
            file_path: "/tmp/skills/lint/SKILL.md".to_string(),
            base_dir: "/tmp/skills/lint".to_string(),
            source_info: crate::core::extensions::SourceInfo::synthetic(
                "/tmp/skills/lint/SKILL.md",
                "local",
                Some("/tmp/skills/lint".to_string()),
            ),
            disable_model_invocation: false,
        }];

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "commands",
                    "type": "get_commands"
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        let commands = response["data"]["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0]["name"], "hello");
        assert_eq!(commands[0]["source"], "extension");
        assert_eq!(commands[0]["sourceInfo"]["baseDir"], "/tmp");
        assert_eq!(commands[1]["name"], "review");
        assert_eq!(commands[1]["source"], "prompt");
        assert_eq!(commands[2]["name"], "skill:lint");
        assert_eq!(commands[2]["source"], "skill");

        let mut dispatch_store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "invoke",
                    "type": "prompt",
                    "message": "/hello first second"
                }))
                .unwrap(),
                &mut dispatch_store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(dispatch_store[0].trim()).unwrap();
        assert_eq!(response["command"], "prompt");
        assert_eq!(response["success"], true);
        assert_eq!(
            *seen_args.lock().unwrap_or_else(|error| error.into_inner()),
            vec!["first second"]
        );
        assert!(runtime.messages.is_empty());
    }

    #[test]
    fn rpc_prompt_catalog_includes_settings_prompt_paths() {
        let root =
            std::env::temp_dir().join(format!("pi-rpc-settings-prompts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let prompt_path = root.join("settings-review.md");
        std::fs::write(
            &prompt_path,
            "---\ndescription: Review from settings\n---\nReview $@\n",
        )
        .unwrap();
        let mut settings = SettingsManager::in_memory(Default::default());
        settings.set_prompt_template_paths(vec![prompt_path.to_string_lossy().into_owned()]);
        let args = Args::default();
        let resources = crate::core::extensions::ResourceDiscovery::default();
        let templates = load_rpc_prompt_templates(
            &args,
            &root.to_string_lossy(),
            &root.join("agent").to_string_lossy(),
            &settings,
            &resources,
        );
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "settings-review");
        assert_eq!(templates[0].description, "Review from settings");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prompt_streaming_behavior_queues_images_and_preserves_template_expansion() {
        let mut runtime = runtime_for_test().await;
        runtime.prompt_templates = vec![crate::core::prompt_templates::PromptTemplate {
            name: "summarize".to_string(),
            description: "Summarize input".to_string(),
            argument_hint: None,
            content: "Please summarize: $@".to_string(),
            source_info: Default::default(),
            file_path: "/tmp/summarize.md".to_string(),
        }];
        runtime.is_streaming = true;
        *runtime
            .run_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "queued",
                    "type": "prompt",
                    "message": "/summarize the diff",
                    "streamingBehavior": "followUp",
                    "images": [{"data": "ZmFrZQ==", "mimeType": "image/png"}]
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[1].trim()).unwrap();
        assert_eq!(response["command"], "prompt");
        assert_eq!(response["success"], true);
        let queued = runtime
            .follow_up_queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot();
        assert_eq!(queued.len(), 1);
        let pi_agent::types::AgentMessage::Core(Message::User(user)) = &queued[0] else {
            panic!("queued prompt was not a user message");
        };
        let pi_ai::types::UserContentBody::Blocks(blocks) = user.content() else {
            panic!("queued prompt did not preserve image blocks");
        };
        assert_eq!(blocks[0], ContentBlock::text("Please summarize: the diff"));
        assert_eq!(blocks[1], ContentBlock::image("ZmFrZQ==", "image/png"));
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
    async fn extension_model_and_tool_requests_update_next_rpc_turn() {
        let mut runtime = runtime_for_test().await;
        // The shared RPC fixture disables tools for hermetic prompt tests;
        // this test specifically exercises the active-tool mutation path, so
        // opt the built-in catalog back in for its isolated runtime.
        runtime.tools_enabled = true;
        runtime.builtin_tools_enabled = true;
        runtime.refresh_extension_catalog();
        let target = runtime
            .models
            .get_model("google", "gemini-2.5-flash")
            .expect("test model is in the builtin catalog");
        runtime
            .loaded_extensions
            .host
            .dispatch_with_outcome(
                crate::core::extensions::ExtensionHostAction::SetModel,
                &serde_json::json!({"model": target}),
            )
            .unwrap();
        runtime
            .loaded_extensions
            .host
            .dispatch_with_outcome(
                crate::core::extensions::ExtensionHostAction::SetActiveTools,
                &serde_json::json!({"toolNames": ["read"]}),
            )
            .unwrap();

        runtime
            .dispatch_pending_extension_actions()
            .await
            .expect("host requests should be applied");

        assert_eq!(runtime.model.provider, "google");
        assert_eq!(runtime.model.id, "gemini-2.5-flash");
        assert_eq!(runtime.loaded_extensions.host.active_tools(), vec!["read"]);
        assert!(runtime
            .loaded_extensions
            .host
            .requested_model_change()
            .is_none());
        assert!(runtime
            .loaded_extensions
            .host
            .requested_active_tools()
            .is_none());

        let prompt = runtime.prepare_prompt_run("next turn", &[]);
        assert_eq!(prompt.config.model.provider, "google");
        assert_eq!(prompt.config.model.id, "gemini-2.5-flash");
        assert_eq!(
            prompt
                .context
                .tools
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read"]
        );
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
        // The faux model does not advertise reasoning, so the upstream
        // session clamps an unsupported requested level to `off`.
        assert_eq!(v["data"]["thinkingLevel"], "off");
    }

    #[tokio::test]
    async fn rpc_applies_settings_to_stream_compaction_retry_and_queues() {
        let mut values = crate::core::settings::SettingsMap::new();
        values.insert("transport".to_string(), serde_json::json!("sse"));
        values.insert(
            "websocketConnectTimeoutMs".to_string(),
            serde_json::json!(333),
        );
        values.insert(
            "compaction".to_string(),
            serde_json::json!({
                "enabled": true,
                "reserveTokens": 1234,
                "keepRecentTokens": 5678
            }),
        );
        values.insert(
            "retry".to_string(),
            serde_json::json!({
                "enabled": true,
                "maxRetries": 4,
                "baseDelayMs": 17,
                "provider": {
                    "timeoutMs": 777,
                    "maxRetries": 5,
                    "maxRetryDelayMs": 888
                }
            }),
        );
        values.insert("steeringMode".to_string(), serde_json::json!("all"));
        values.insert(
            "followUpMode".to_string(),
            serde_json::json!("one-at-a-time"),
        );
        values.insert(
            "thinkingBudgets".to_string(),
            serde_json::json!({"minimal": 111, "low": 222, "medium": 333, "high": 444}),
        );
        let mut runtime = runtime_for_test_with_settings(SettingsManager::in_memory(values)).await;

        assert_eq!(runtime.steering_mode, "all");
        assert_eq!(runtime.follow_up_mode, "one-at-a-time");
        assert_eq!(
            runtime
                .steering_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .mode,
            QueueMode::All
        );
        assert_eq!(
            runtime
                .follow_up_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .mode,
            QueueMode::OneAtATime
        );

        // Faux is intentionally non-reasoning, so make the request model
        // reasoning-capable to inspect the configured request-level effort.
        runtime.model.reasoning = true;
        runtime.thinking_level = pi_ai::types::ModelThinkingLevel::High;
        let options = runtime.runtime_simple_stream_options();
        assert_eq!(options.base.transport.as_deref(), Some("sse"));
        assert_eq!(options.base.base.timeout_ms, Some(777));
        assert_eq!(options.base.base.max_retries, Some(5));
        assert_eq!(options.base.base.max_retry_delay_ms, Some(888));
        assert_eq!(options.base.websocket_connect_timeout_ms, Some(333));
        assert_eq!(options.reasoning, Some(pi_ai::types::ThinkingLevel::High));
        assert_eq!(
            options.thinking_budgets,
            Some(pi_ai::types::ThinkingBudgets {
                minimal: Some(111),
                low: Some(222),
                medium: Some(333),
                high: Some(444),
            })
        );

        let compaction = runtime.compaction_settings();
        assert_eq!(compaction.reserve_tokens, 1234);
        assert_eq!(compaction.keep_recent_tokens, 5678);
        let retry = runtime.retry_policy();
        assert!(retry.enabled);
        assert_eq!(retry.max_retries, 4);
        assert_eq!(retry.base_delay_ms, 17);
    }

    #[tokio::test]
    async fn rpc_runtime_control_commands_update_settings_and_state() {
        let mut runtime = runtime_for_test().await;
        let commands = [
            serde_json::json!({
                "id": "compact-off",
                "type": "set_auto_compaction",
                "enabled": false
            }),
            serde_json::json!({
                "id": "retry-off",
                "type": "set_auto_retry",
                "enabled": false
            }),
            serde_json::json!({
                "id": "steer-all",
                "type": "set_steering_mode",
                "mode": "all"
            }),
            serde_json::json!({
                "id": "follow-one",
                "type": "set_follow_up_mode",
                "mode": "one-at-a-time"
            }),
        ];
        let mut store = Vec::new();
        for command in commands {
            runtime
                .handle_command(RpcCommand::parse(command).unwrap(), &mut store)
                .await
                .unwrap();
        }

        assert!(store.iter().all(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            value["type"] == "response" && value["success"] == true
        }));
        assert!(!runtime.auto_compaction_enabled);
        assert!(!runtime.auto_retry_enabled);
        assert!(!runtime.settings.get_compaction_enabled());
        assert!(!runtime.settings.get_retry_enabled());
        assert_eq!(runtime.steering_mode, "all");
        assert_eq!(runtime.follow_up_mode, "one-at-a-time");
        assert_eq!(
            runtime
                .steering_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .mode,
            QueueMode::All
        );
        assert_eq!(
            runtime
                .follow_up_queue
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .mode,
            QueueMode::OneAtATime
        );

        let mut state = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "get_state"})).unwrap(),
                &mut state,
            )
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(state[0].trim()).unwrap();
        assert_eq!(value["data"]["autoCompactionEnabled"], false);
        assert_eq!(value["data"]["steeringMode"], "all");
        assert_eq!(value["data"]["followUpMode"], "one-at-a-time");
    }

    #[tokio::test]
    async fn bash_executes_and_captures() {
        let abort = Arc::new(AtomicBool::new(false));
        let result = run_bash("echo hello-rpc", "/tmp", abort).await;
        assert_eq!(result["output"], "hello-rpc\n");
        assert_eq!(result["exitCode"], 0);
    }

    #[tokio::test]
    async fn abort_bash_interrupts_silent_process_and_abort_does_not_target_it() {
        let mut runtime = runtime_for_test().await;
        let bash_abort = runtime.abort_bash.clone();
        let cwd = runtime.cwd.clone();
        let bash = tokio::spawn(async move { run_bash("sleep 10", &cwd, bash_abort).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "abort"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        assert!(!runtime.abort_bash.load(Ordering::SeqCst));

        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({"type": "abort_bash"})).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), bash)
            .await
            .expect("abort_bash should interrupt a silent process")
            .unwrap();
        assert_eq!(result["cancelled"], true);
        assert!(result["exitCode"].is_null());
    }

    #[tokio::test]
    async fn standalone_bash_result_is_persisted_with_context_flag() {
        let mut runtime = runtime_for_test().await;
        let mut store = Vec::new();
        runtime
            .handle_command(
                RpcCommand::parse(serde_json::json!({
                    "id": "bash-1",
                    "type": "bash",
                    "command": "echo recorded",
                    "excludeFromContext": true
                }))
                .unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(store[0].trim()).unwrap();
        assert_eq!(response["success"], true);
        assert_eq!(response["data"]["output"], "recorded\n");

        let entries = runtime.get_entries().await.unwrap();
        assert!(entries.iter().any(|entry| {
            matches!(
                entry.as_message(),
                Some(pi_agent::types::AgentMessage::Custom(
                    pi_agent::types::CustomAgentMessage::BashExecution {
                        exclude_from_context: Some(true),
                        ..
                    }
                ))
            )
        }));
    }

    #[tokio::test]
    async fn bash_records_defer_until_prompt_settles() {
        let mut runtime = runtime_for_test().await;
        runtime.is_streaming = true;
        runtime
            .record_bash_result(
                "echo deferred",
                &serde_json::json!({
                    "output": "deferred\n",
                    "exitCode": 0,
                    "cancelled": false,
                    "truncated": false
                }),
                None,
            )
            .await
            .unwrap();
        assert!(runtime.messages.is_empty());
        assert_eq!(runtime.pending_bash_messages.len(), 1);

        runtime.is_streaming = false;
        runtime.flush_pending_bash_messages().await.unwrap();
        assert!(runtime.pending_bash_messages.is_empty());
        assert!(matches!(
            runtime.messages.first(),
            Some(pi_agent::types::AgentMessage::Custom(
                pi_agent::types::CustomAgentMessage::BashExecution { .. }
            ))
        ));
        assert_eq!(runtime.get_entries().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn detached_prompt_emits_lifecycle_and_tool_terminal_events() {
        let root = std::env::temp_dir().join(format!("pi-rpc-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let core = pi_ai::providers::FauxProviderCore::new(
            &pi_ai::providers::RegisterFauxProviderOptions::default(),
        );
        core.set_responses(vec![
            pi_ai::providers::FauxResponseStep::Message(pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::tool_call(
                    "tool-1",
                    "bash",
                    serde_json::json!({"command": "echo from-rpc"}),
                )],
                pi_ai::providers::FauxAssistantOptions {
                    stop_reason: Some(pi_ai::types::StopReason::ToolUse),
                    ..Default::default()
                },
            )),
            pi_ai::providers::FauxResponseStep::Message(pi_ai::providers::faux_assistant_message(
                vec![pi_ai::types::ContentBlock::text("finished")],
                pi_ai::providers::FauxAssistantOptions::default(),
            )),
        ]);
        let model = core.get_model(None).unwrap().clone();
        let stream_core = core.clone();
        let stream_fn: pi_agent::agent::StreamFn =
            Arc::new(move |model, context| stream_core.stream(model, context, None));
        let context = AgentContext::new(
            Some("test".to_string()),
            vec![pi_agent::tools::bash_tool(
                root.to_string_lossy().into_owned(),
            )],
        );
        let config = RichAgentLoopConfig::new(model, stream_fn, None);
        let run = RpcPromptRun {
            prompts: vec![pi_agent::agent::user_text_prompt("hello", 1)],
            context,
            config,
            provider_uses_oauth: false,
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
                RpcPromptTaskMessage::ExtensionCommandFinished { .. } => {
                    panic!("prompt worker emitted an extension-command completion")
                }
            }
        }
        let result = result.expect("prompt should settle");
        assert!(
            result.persisted_messages.len() >= 4,
            "persisted messages: {:?}",
            result.persisted_messages
        );
        let values: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line.trim()).unwrap())
            .collect();
        for event_type in [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "turn_end",
            "agent_end",
        ] {
            assert!(
                values.iter().any(|value| value["type"] == event_type),
                "missing {event_type} in {values:?}"
            );
        }
        assert!(values.iter().any(|value| {
            value["type"] == "tool_execution_start"
                && value["toolCallId"] == "tool-1"
                && value["toolName"] == "bash"
        }));
        assert!(values.iter().any(|value| {
            value["type"] == "message_end" && value["message"]["role"] == "toolResult"
        }));
        let _ = std::fs::remove_dir_all(root);
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
        let v: serde_json::Value = store
            .iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).unwrap())
            .find(|value| value["type"] == "response")
            .unwrap();
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
        let control_values = controls
            .iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            control_values
                .iter()
                .filter(|value| value["type"] == "response")
                .count(),
            5
        );
        assert_eq!(
            control_values
                .iter()
                .filter(|value| value["type"] == "queue_update")
                .count(),
            2
        );
        assert!(control_values
            .iter()
            .filter(|value| value["type"] == "response")
            .all(|value| value["success"] == true));
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
            .settle_prompt_with_persistence(
                new_messages.clone(),
                plain_persisted_messages(new_messages),
            )
            .await;
        assert!(!runtime.is_streaming);
        assert!(!*runtime
            .run_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner()));
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
    async fn rpc_load_session_migrates_legacy_v3_file() {
        let mut runtime = runtime_for_test().await;
        let current_path = runtime.session_path.clone().expect("session path exists");
        let legacy_path = std::path::Path::new(&current_path).with_file_name("legacy-switch.jsonl");
        let legacy = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"legacy-switch\",\"timestamp\":\"2026-08-22T00:00:00.000Z\",\"cwd\":{}}}\n{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-08-22T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"legacy prompt\"}}}}\n",
            serde_json::to_string(&runtime.cwd).unwrap()
        );
        std::fs::write(&legacy_path, legacy).unwrap();

        runtime
            .load_session(&legacy_path.to_string_lossy())
            .await
            .expect("legacy switch should migrate and open");

        assert_eq!(runtime.session_id, "legacy-switch");
        assert!(std::fs::read_to_string(&legacy_path)
            .unwrap()
            .starts_with("{\"kind\":\"header\""));
        let entries = runtime.get_entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        let Some(pi_agent::types::AgentMessage::Core(Message::User(user))) =
            entries[0].as_message()
        else {
            panic!("expected migrated user message");
        };
        assert_eq!(pi_agent::agent::user_content_text(user), "legacy prompt");
        let _ = std::fs::remove_file(legacy_path);
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
    async fn malformed_rpc_lines_emit_parse_failures() {
        use tokio::io::AsyncReadExt;

        let mut runtime = runtime_for_test().await;
        let (mut writer, mut reader) = tokio::io::duplex(8192);
        let (task_events, _task_receiver) = mpsc::unbounded_channel();
        let mut prompt_active = false;
        let mut active_bashes = 0;
        let mut pending_commands = VecDeque::new();
        let mut pending_abort_responses = VecDeque::new();

        for line in [
            "{not-json",
            r#"{"id":"missing-type"}"#,
            r#"{"id":"ui","type":"extension_ui_response","result":{"confirmed":true}}"#,
        ] {
            let mut state = RpcDispatchState {
                out: &mut writer,
                task_events: &task_events,
                prompt_active: &mut prompt_active,
                active_bashes: &mut active_bashes,
                pending_commands: &mut pending_commands,
                pending_abort_responses: &mut pending_abort_responses,
            };
            dispatch_rpc_line(&mut runtime, line.to_string(), &mut state)
                .await
                .unwrap();
        }
        drop(writer);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        let records = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        for record in records.iter().take(2) {
            assert_eq!(record["type"], "response");
            assert_eq!(record["command"], "parse");
            assert_eq!(record["success"], false);
            assert!(record.get("id").is_none());
        }
        assert_eq!(records[2]["type"], "extension_ui_diagnostic");
        assert_eq!(records[2]["code"], "unknown_response_id");
        assert_eq!(records[2]["id"], "ui");
    }

    #[tokio::test]
    async fn rpc_dispatch_routes_select_confirm_input_and_editor_responses() {
        let mut runtime = runtime_for_test().await;
        let host = runtime.loaded_extensions.host.clone();
        host.set_ui_enabled(true);
        let broker = host.ui_broker();

        let select_context =
            crate::core::extensions::types::ExtensionUiContext::new(broker.clone(), true, true);
        let select = std::thread::spawn(move || {
            select_context.select(
                "Choose",
                &["one".to_string(), "two".to_string()],
                Some(std::time::Duration::from_secs(1)),
            )
        });
        let request = next_test_ui_request(&host);
        assert_eq!(request["method"], "select");
        dispatch_test_ui_response(
            &mut runtime,
            serde_json::json!({
                "type": "extension_ui_response",
                "id": request["id"],
                "value": "two"
            }),
        )
        .await
        .expect("select response route");
        assert_eq!(
            select.join().expect("select worker"),
            Ok(Some("two".to_string()))
        );

        let confirm_context =
            crate::core::extensions::types::ExtensionUiContext::new(broker.clone(), true, true);
        let confirm = std::thread::spawn(move || {
            confirm_context.confirm(
                "Continue",
                "Continue the operation?",
                Some(std::time::Duration::from_secs(1)),
            )
        });
        let request = next_test_ui_request(&host);
        assert_eq!(request["method"], "confirm");
        dispatch_test_ui_response(
            &mut runtime,
            serde_json::json!({
                "type": "extension_ui_response",
                "id": request["id"],
                "result": true
            }),
        )
        .await
        .expect("confirm response route");
        assert_eq!(confirm.join().expect("confirm worker"), Ok(true));

        let input_context =
            crate::core::extensions::types::ExtensionUiContext::new(broker.clone(), true, true);
        let input = std::thread::spawn(move || {
            input_context.input(
                "Name",
                Some("placeholder"),
                Some(std::time::Duration::from_secs(1)),
            )
        });
        let request = next_test_ui_request(&host);
        assert_eq!(request["method"], "input");
        dispatch_test_ui_response(
            &mut runtime,
            serde_json::json!({
                "type": "extension_ui_response",
                "id": request["id"],
                "value": "Ada"
            }),
        )
        .await
        .expect("input response route");
        assert_eq!(
            input.join().expect("input worker"),
            Ok(Some("Ada".to_string()))
        );

        let editor_context =
            crate::core::extensions::types::ExtensionUiContext::new(broker, true, true);
        let editor = std::thread::spawn(move || editor_context.editor("Edit", Some("prefill")));
        let request = next_test_ui_request(&host);
        assert_eq!(request["method"], "editor");
        dispatch_test_ui_response(
            &mut runtime,
            serde_json::json!({
                "type": "extension_ui_response",
                "id": request["id"],
                "result": "edited"
            }),
        )
        .await
        .expect("editor response route");
        assert_eq!(
            editor.join().expect("editor worker"),
            Ok(Some("edited".to_string()))
        );
        assert_eq!(host.snapshot()["editorText"], "edited");
    }

    #[tokio::test]
    async fn rpc_dispatch_routes_terminal_input_through_the_live_extension_broker() {
        use tokio::io::AsyncReadExt;

        let mut runtime = runtime_for_test().await;
        let host = runtime.loaded_extensions.host.clone();
        host.set_ui_enabled(true);
        let context =
            crate::core::extensions::types::ExtensionUiContext::new(host.ui_broker(), true, false);
        let _subscription = context
            .on_terminal_input(Arc::new(|input| {
                Ok(Some(
                    crate::core::extensions::types::TerminalInputHandlerResult {
                        consume: Some(true),
                        data: Some(format!("{input}-handled")),
                    },
                ))
            }))
            .expect("terminal input listener registration");

        let (mut writer, mut reader) = tokio::io::duplex(8192);
        let (task_events, _task_receiver) = mpsc::unbounded_channel();
        let mut prompt_active = false;
        let mut active_bashes = 0;
        let mut pending_commands = VecDeque::new();
        let mut pending_abort_responses = VecDeque::new();
        {
            let mut state = RpcDispatchState {
                out: &mut writer,
                task_events: &task_events,
                prompt_active: &mut prompt_active,
                active_bashes: &mut active_bashes,
                pending_commands: &mut pending_commands,
                pending_abort_responses: &mut pending_abort_responses,
            };
            dispatch_rpc_line(
                &mut runtime,
                r#"{"type":"extension_ui_input","id":"terminal-1","data":"raw"}"#.to_string(),
                &mut state,
            )
            .await
            .expect("terminal input dispatch");
        }
        drop(writer);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.expect("RPC output");
        let response: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&output).unwrap().trim())
                .expect("terminal input response JSON");
        assert_eq!(response["type"], "extension_ui_input_result");
        assert_eq!(response["id"], "terminal-1");
        assert_eq!(response["data"], "raw-handled");
        assert_eq!(response["consumed"], true);
        assert_eq!(response["listenerCount"], 1);
        assert_eq!(response["errorCount"], 0);
    }

    #[tokio::test]
    async fn rpc_extension_command_forwards_real_ui_request_and_routes_response() {
        let mut runtime = runtime_for_test().await;
        let shared_runtime = runtime.loaded_extensions.runtime.clone();
        let extension = crate::core::extensions::loader::load_extension_from_factory(
            |api| {
                api.register_command(
                    "ask",
                    Some("ask the host".to_string()),
                    Arc::new(|context, _event| {
                        let answer = context.ui.input(
                            "Question",
                            Some("answer"),
                            Some(std::time::Duration::from_secs(1)),
                        )?;
                        Ok(Some(serde_json::json!({"answer": answer})))
                    }),
                )?;
                Ok(())
            },
            &runtime.cwd,
            shared_runtime.clone(),
            "<inline:rpc-ui>",
        )
        .expect("RPC UI extension factory");
        let host = Arc::new(crate::core::extensions::ExtensionHostState::new(
            None, "medium",
        ));
        host.set_ui_enabled(true);
        let mut runner = crate::core::extensions::ExtensionRunner::new(
            vec![extension],
            shared_runtime.clone(),
            runtime.cwd.clone(),
        );
        runner.set_ui_context("rpc", true);
        runner.bind_core_with_actions(host.clone());
        runtime.loaded_extensions = LoadedExtensions {
            runner: Arc::new(runner),
            host,
            runtime: shared_runtime,
            errors: Vec::new(),
            resources: Default::default(),
        };

        let (task_events, mut task_receiver) = mpsc::unbounded_channel();
        let ui_task_events = task_events.clone();
        runtime.bind_ui_request_sink(Arc::new(move |request| {
            ui_task_events
                .send(RpcTaskMessage::ExtensionUiRequest(serialize_json_line(
                    &request,
                )))
                .map_err(|_| "RPC task event channel closed".to_string())
        }));
        let mut preflight = Vec::new();
        let mut completion_receiver = runtime
            .start_prompt_task(
                RpcCommand::parse(serde_json::json!({
                    "id": "ask-1",
                    "type": "prompt",
                    "message": "/ask from rpc"
                }))
                .unwrap(),
                &mut preflight,
            )
            .expect("extension command should start a worker");
        assert!(preflight.is_empty());

        let request_line = match task_receiver.recv().await.expect("UI task event") {
            RpcTaskMessage::ExtensionUiRequest(line) => line,
            RpcTaskMessage::Prompt(_) => panic!("prompt event before UI request"),
            RpcTaskMessage::BashUpdate(_) | RpcTaskMessage::Bash(_) => {
                panic!("bash event before UI request")
            }
        };
        let request: serde_json::Value = serde_json::from_str(request_line.trim()).unwrap();
        assert_eq!(request["type"], "extension_ui_request");
        assert_eq!(request["method"], "input");
        assert_eq!(request["title"], "Question");

        let mut output = tokio::io::sink();
        let mut prompt_active = true;
        let mut active_bashes = 0;
        let mut pending_commands = VecDeque::new();
        let mut pending_abort_responses = VecDeque::new();
        {
            let mut dispatch_state = RpcDispatchState {
                out: &mut output,
                task_events: &task_events,
                prompt_active: &mut prompt_active,
                active_bashes: &mut active_bashes,
                pending_commands: &mut pending_commands,
                pending_abort_responses: &mut pending_abort_responses,
            };
            dispatch_rpc_line(
                &mut runtime,
                serialize_json_line(&serde_json::json!({
                    "type": "extension_ui_response",
                    "id": request["id"],
                    "result": "real answer"
                })),
                &mut dispatch_state,
            )
            .await
            .expect("RPC response should resolve the worker");
        }

        match completion_receiver
            .recv()
            .await
            .expect("extension completion")
        {
            RpcPromptTaskMessage::ExtensionCommandFinished {
                id,
                command,
                result: Ok(Some(value)),
            } => {
                assert_eq!(id.as_deref(), Some("ask-1"));
                assert_eq!(command, "prompt");
                assert_eq!(value, serde_json::json!({"answer": "real answer"}));
            }
            RpcPromptTaskMessage::ExtensionCommandFinished { result, .. } => {
                panic!("unexpected extension command result: {result:?}")
            }
            RpcPromptTaskMessage::Event(_) | RpcPromptTaskMessage::Finished(_) => {
                panic!("extension command emitted a prompt result")
            }
        }
    }

    #[tokio::test]
    async fn rpc_dispatch_reports_unknown_and_late_ui_responses() {
        use tokio::io::AsyncReadExt;

        let mut runtime = runtime_for_test().await;
        let broker = runtime.loaded_extensions.host.ui_broker();
        broker.set_enabled(true);
        let context =
            crate::core::extensions::types::ExtensionUiContext::new(broker.clone(), true, true);
        let worker = std::thread::spawn(move || {
            context.input(
                "Short timeout",
                None,
                Some(std::time::Duration::from_millis(10)),
            )
        });
        let request = (0..500)
            .find_map(|_| {
                runtime
                    .loaded_extensions
                    .host
                    .drain_ui_requests()
                    .into_iter()
                    .next()
                    .or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        None
                    })
            })
            .expect("timed request");
        let id = request["id"]
            .as_str()
            .expect("timed request id")
            .to_string();
        assert_eq!(worker.join().expect("timed UI worker"), Ok(None));

        let (mut writer, mut reader) = tokio::io::duplex(8192);
        let (task_events, _task_receiver) = mpsc::unbounded_channel();
        let mut prompt_active = false;
        let mut active_bashes = 0;
        let mut pending_commands = VecDeque::new();
        let mut pending_abort_responses = VecDeque::new();
        for response in [
            serde_json::json!({
                "type": "extension_ui_response",
                "id": "unknown",
                "value": "no request"
            }),
            serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "value": "after timeout"
            }),
        ] {
            let mut state = RpcDispatchState {
                out: &mut writer,
                task_events: &task_events,
                prompt_active: &mut prompt_active,
                active_bashes: &mut active_bashes,
                pending_commands: &mut pending_commands,
                pending_abort_responses: &mut pending_abort_responses,
            };
            dispatch_rpc_line(&mut runtime, serialize_json_line(&response), &mut state)
                .await
                .expect("diagnostic response dispatch");
        }
        drop(writer);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        let records: Vec<_> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect();
        assert_eq!(
            records
                .iter()
                .map(|record| record["code"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["unknown_response_id", "late_response"]
        );
    }

    #[tokio::test]
    async fn rpc_task_handler_forwards_extension_ui_requests_while_idle() {
        use tokio::io::AsyncReadExt;

        let mut runtime = runtime_for_test().await;
        let (mut writer, mut reader) = tokio::io::duplex(8192);
        let mut prompt_active = false;
        let mut active_bashes = 0;
        let mut pending_abort_responses = VecDeque::new();
        handle_rpc_task_message(
            &mut runtime,
            Some(RpcTaskMessage::ExtensionUiRequest(
                "{\"type\":\"extension_ui_request\",\"id\":\"idle-1\",\"method\":\"notify\",\"message\":\"hello\"}\n".to_string(),
            )),
            &mut writer,
            &mut prompt_active,
            &mut active_bashes,
            &mut pending_abort_responses,
        )
        .await
        .expect("idle UI event forwarding");
        drop(writer);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        let request: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&output).unwrap().trim()).unwrap();
        assert_eq!(request["method"], "notify");
        assert_eq!(request["id"], "idle-1");
    }

    #[tokio::test]
    async fn rpc_extension_reload_rebinds_the_ui_request_sink() {
        let mut runtime = runtime_for_test().await;
        let (sender, receiver) = std::sync::mpsc::channel();
        runtime.bind_ui_request_sink(Arc::new(move |request| {
            sender
                .send(request)
                .map_err(|_| "UI request receiver closed".to_string())
        }));
        runtime
            .install_replacement_extensions("test-reload", None, BTreeMap::new())
            .expect("reload extension installation");

        runtime
            .loaded_extensions
            .host
            .ui_broker()
            .notify("after reload", None)
            .expect("post-reload UI notification");
        let request = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("replacement host should use the retained UI sink");
        assert_eq!(request["type"], "extension_ui_request");
        assert_eq!(request["method"], "notify");
        assert_eq!(request["message"], "after reload");
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
        assert!(html.contains("Static zero-JavaScript export"));
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
            if v["type"] == "message_end" {
                if let Some(text) = v["message"]["content"].as_array() {
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

    fn model_signature(value: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "provider": value.get("provider").cloned().unwrap_or(serde_json::Value::Null),
            "id": value.get("id").cloned().unwrap_or(serde_json::Value::Null),
        })
    }

    fn data_signature(command: &str, data: &serde_json::Value) -> serde_json::Value {
        match command {
            "get_state" => {
                let mut value = data.clone();
                value["model"] = model_signature(&data["model"]);
                value["sessionFile"] = if data["sessionFile"].is_null() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!("<session-file>")
                };
                value["sessionId"] = serde_json::json!("<session-id>");
                value
            }
            "set_model" => model_signature(data),
            "cycle_model" => {
                if data.is_null() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!({
                        "model": model_signature(&data["model"]),
                        "thinkingLevel": data["thinkingLevel"],
                        "isScoped": data["isScoped"],
                    })
                }
            }
            "get_available_models" => {
                let models = data["models"].as_array().cloned().unwrap_or_default();
                serde_json::json!({
                    "modelCount": models.len(),
                    "firstModel": models.first().map(model_signature),
                    "lastModel": models.last().map(model_signature),
                })
            }
            "compact" => {
                let mut value = data.clone();
                if value.get("firstKeptEntryId").is_some() {
                    value["firstKeptEntryId"] = serde_json::json!("<entry-id>");
                }
                normalize_compaction_costs(&mut value);
                value
            }
            "get_session_stats" => {
                let mut value = data.clone();
                value["sessionFile"] = serde_json::json!("<session-file>");
                value["sessionId"] = serde_json::json!("<session-id>");
                value
            }
            "export_html" => serde_json::json!({ "path": "<html-path>" }),
            "fork" => serde_json::json!({
                "text": data["text"],
                "cancelled": data["cancelled"],
            }),
            "new_session" | "switch_session" | "clone" => data.clone(),
            "get_fork_messages" => serde_json::json!({
                "messages": data["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|message| serde_json::json!({
                        "entryId": "<entry-id>",
                        "text": message["text"],
                    }))
                    .collect::<Vec<_>>(),
            }),
            "get_entries" => serde_json::json!({
                "entryTypes": data["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry["type"].as_str())
                    .collect::<Vec<_>>(),
                "entryCount": data["entries"].as_array().map_or(0, Vec::len),
                "leafId": data["leafId"].as_str().map_or_else(
                    || serde_json::Value::Null,
                    |_| serde_json::json!("<entry-id>"),
                ),
            }),
            "get_tree" => serde_json::json!({
                "rootCount": data["tree"].as_array().map_or(0, Vec::len),
                "rootChildCounts": data["tree"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|node| node["children"].as_array().map_or(0, Vec::len))
                    .collect::<Vec<_>>(),
                "leafId": data["leafId"].as_str().map_or_else(
                    || serde_json::Value::Null,
                    |_| serde_json::json!("<entry-id>"),
                ),
            }),
            "get_messages" => serde_json::json!({
                "roles": data["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|message| message["role"].as_str())
                    .collect::<Vec<_>>(),
                "messageCount": data["messages"].as_array().map_or(0, Vec::len),
            }),
            _ => data.clone(),
        }
    }

    fn normalize_compaction_costs(value: &mut serde_json::Value) {
        if let Some(cost) = value
            .get_mut("usage")
            .and_then(|usage| usage.get_mut("cost"))
            .and_then(serde_json::Value::as_object_mut)
        {
            for amount in cost.values_mut() {
                if amount.as_f64() == Some(0.0) {
                    *amount = serde_json::json!(0);
                }
            }
        }
    }

    fn normalize_rpc_error(error: &str) -> String {
        fn is_uuid(value: &str) -> bool {
            value.len() == 36
                && value.chars().enumerate().all(|(index, character)| {
                    character.is_ascii_hexdigit()
                        || matches!(index, 8 | 13 | 18 | 23) && character == '-'
                })
        }

        error
            .split_whitespace()
            .map(|token| {
                if ["model-", "thinking-"]
                    .iter()
                    .any(|prefix| token.strip_prefix(prefix).is_some_and(is_uuid))
                {
                    "<entry-id>"
                } else {
                    token
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn rpc_wire_signature(value: &serde_json::Value) -> serde_json::Value {
        if value["type"] == "response" {
            let mut signature = serde_json::json!({
                "id": value.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "type": "response",
                "command": value["command"],
                "success": value["success"],
            });
            if value["success"] == true {
                if let Some(data) = value.get("data") {
                    signature["data"] = data_signature(value["command"].as_str().unwrap(), data);
                }
            } else {
                signature["error"] = value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(normalize_rpc_error)
                    .map(serde_json::Value::String)
                    .unwrap_or_else(|| value["error"].clone());
            }
            return signature;
        }

        match value["type"].as_str().unwrap_or_default() {
            "message_start" | "message_end" => serde_json::json!({
                "type": value["type"],
                "role": value["message"]["role"],
                "stopReason": value["message"].get("stopReason").cloned().unwrap_or(serde_json::Value::Null),
            }),
            "message_update" => serde_json::json!({
                "type": "message_update",
                "assistantEvent": value["assistantMessageEvent"],
            }),
            "turn_end" => serde_json::json!({
                "type": "turn_end",
                "role": value["message"]["role"],
                "toolResultCount": value["toolResults"].as_array().map_or(0, Vec::len),
            }),
            "agent_end" => serde_json::json!({
                "type": "agent_end",
                "messageCount": value["messages"].as_array().map_or(0, Vec::len),
                "willRetry": value["willRetry"],
            }),
            "compaction_end" => {
                let mut value = value.clone();
                if value["result"].get("firstKeptEntryId").is_some() {
                    value["result"]["firstKeptEntryId"] = serde_json::json!("<entry-id>");
                }
                normalize_compaction_costs(&mut value["result"]);
                value
            }
            "tool_execution_start" => serde_json::json!({
                "type": value["type"],
                "toolCallId": value["toolCallId"],
                "toolName": value["toolName"],
                "args": value["args"],
            }),
            "tool_execution_update" => serde_json::json!({
                "type": value["type"],
                "toolCallId": value["toolCallId"],
                "toolName": value["toolName"],
                "args": value["args"],
                "partialResult": value["partialResult"],
            }),
            "tool_execution_end" => serde_json::json!({
                "type": value["type"],
                "toolCallId": value["toolCallId"],
                "toolName": value["toolName"],
                "resultContentCount": value["result"]["content"].as_array().map_or(0, Vec::len),
                "terminate": value["result"].get("terminate").cloned().unwrap_or(serde_json::Value::Bool(false)),
                "isError": value["isError"],
            }),
            _ => value.clone(),
        }
    }

    fn rpc_event_signature(value: &serde_json::Value) -> serde_json::Value {
        if value["type"] != "message_update" {
            return rpc_wire_signature(value);
        }
        let inner = &value["assistantMessageEvent"];
        let mut signature = serde_json::json!({
            "type": "message_update",
            "assistantEventType": inner["type"],
        });
        for key in [
            "contentIndex",
            "delta",
            "content",
            "id",
            "toolName",
            "reason",
            "toolCall",
        ] {
            if let Some(value) = inner.get(key) {
                signature[key] = value.clone();
            }
        }
        if let Some(message) = inner.get("message") {
            signature["messageRole"] = message["role"].clone();
            signature["messageContentCount"] =
                serde_json::json!(message["content"].as_array().map_or(0, Vec::len));
        }
        signature
    }

    fn golden_assistant(
        content: Vec<pi_ai::types::ContentBlock>,
    ) -> pi_ai::types::AssistantMessage {
        let mut message = pi_ai::types::AssistantMessage::new().with_timestamp(7);
        message.set_api_provider_model("faux", "faux", "faux-1");
        message.set_usage(pi_ai::types::Usage::default());
        message.set_content(content);
        message.set_stop_reason(pi_ai::types::StopReason::Stop);
        message
    }

    async fn capture_rpc_command(
        runtime: &mut RpcRuntime,
        case_name: &str,
        input: serde_json::Value,
        transcript: &mut Vec<serde_json::Value>,
    ) {
        let command = RpcCommand::parse(input).unwrap();
        let command_name = command.type_.clone();
        let mut store = Vec::new();
        runtime.handle_command(command, &mut store).await.unwrap();
        let records = store
            .iter()
            .map(|line| {
                rpc_wire_signature(&serde_json::from_str::<serde_json::Value>(line.trim()).unwrap())
            })
            .collect::<Vec<_>>();
        transcript.push(serde_json::json!({
            "case": case_name,
            "command": command_name,
            "records": records,
        }));
    }

    #[test]
    fn rpc_event_golden_transcript_covers_wire_event_types() {
        let assistant = golden_assistant(vec![pi_ai::types::ContentBlock::text("hello")]);
        let tool_call = pi_ai::types::ContentBlock::tool_call(
            "call-1",
            "bash",
            serde_json::json!({"command":"echo hi"}),
        );
        let tool_result = pi_ai::types::ToolResultMessage::text("call-1", "bash", "hi", false)
            .with_details_usage_timestamp(None, None, 7);
        let mut lifecycle_result = pi_agent::tools::AgentToolResult::output("hi");
        lifecycle_result.terminate = true;
        let user = pi_agent::types::AgentMessage::Core(Message::User(
            pi_ai::types::UserContent::string("hello", 7),
        ));
        let assistant_message =
            pi_agent::types::AgentMessage::Core(Message::Assistant(assistant.clone()));
        let partial_text = golden_assistant(vec![pi_ai::types::ContentBlock::text("hello")]);
        let partial_thinking =
            golden_assistant(vec![pi_ai::types::ContentBlock::thinking("reason")]);
        let partial_tool = golden_assistant(vec![tool_call.clone()]);
        let events = vec![
            ("agent_start", RichAgentEvent::AgentStart),
            (
                "agent_end",
                RichAgentEvent::AgentEnd {
                    messages: vec![user.clone(), assistant_message.clone()],
                },
            ),
            (
                "auto_retry_start",
                RichAgentEvent::AutoRetryStart {
                    attempt: 1,
                    max_attempts: 3,
                    delay_ms: 25,
                    error_message: "overloaded".to_string(),
                },
            ),
            (
                "auto_retry_end",
                RichAgentEvent::AutoRetryEnd {
                    success: false,
                    attempt: 1,
                    final_error: Some("overloaded".to_string()),
                },
            ),
            ("turn_start", RichAgentEvent::TurnStart),
            (
                "turn_end",
                RichAgentEvent::TurnEnd {
                    message: assistant_message.clone(),
                    tool_results: vec![tool_result.clone()],
                },
            ),
            (
                "message_start",
                RichAgentEvent::MessageStart {
                    message: user.clone(),
                },
            ),
            (
                "message_update.start",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::Start {
                        partial: partial_text.clone(),
                    },
                },
            ),
            (
                "message_update.text_start",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: partial_text.clone(),
                    },
                },
            ),
            (
                "message_update.text_delta",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "he".to_string(),
                        partial: partial_text.clone(),
                    },
                },
            ),
            (
                "message_update.text_end",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::TextEnd {
                        content_index: 0,
                        content: "hello".to_string(),
                        partial: partial_text.clone(),
                    },
                },
            ),
            (
                "message_update.thinking_start",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::ThinkingStart {
                        content_index: 0,
                        partial: partial_thinking.clone(),
                    },
                },
            ),
            (
                "message_update.thinking_delta",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                        content_index: 0,
                        delta: "rea".to_string(),
                        partial: partial_thinking.clone(),
                    },
                },
            ),
            (
                "message_update.thinking_end",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::ThinkingEnd {
                        content_index: 0,
                        content: "reason".to_string(),
                        partial: partial_thinking.clone(),
                    },
                },
            ),
            (
                "message_update.toolcall_start",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::ToolCallStart {
                        content_index: 0,
                        partial: partial_tool.clone(),
                    },
                },
            ),
            (
                "message_update.toolcall_delta",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::ToolCallDelta {
                        content_index: 0,
                        delta: "{\"command\":\"echo hi\"}".to_string(),
                        partial: partial_tool.clone(),
                    },
                },
            ),
            (
                "message_update.toolcall_end",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::ToolCallEnd {
                        content_index: 0,
                        tool_call: tool_call.clone(),
                        partial: partial_tool.clone(),
                    },
                },
            ),
            (
                "message_update.done",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::Done {
                        reason: pi_ai::types::DoneReason::Stop,
                        message: assistant.clone(),
                    },
                },
            ),
            (
                "message_update.error",
                RichAgentEvent::MessageUpdate {
                    message: assistant_message.clone(),
                    assistant_message_event: AssistantMessageEvent::Error {
                        reason: pi_ai::types::ErrorReason::Error,
                        error_message: assistant.clone(),
                    },
                },
            ),
            (
                "message_end",
                RichAgentEvent::MessageEnd {
                    message: assistant_message.clone(),
                },
            ),
            (
                "tool_execution_start",
                RichAgentEvent::ToolExecutionStart {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "bash".to_string(),
                    args: serde_json::json!({"command":"echo hi"}),
                },
            ),
            (
                "tool_execution_update",
                RichAgentEvent::ToolExecutionUpdate {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "bash".to_string(),
                    args: serde_json::json!({"command":"echo hi"}),
                    partial_result: serde_json::json!({"output":"h"}),
                },
            ),
            (
                "tool_execution_end",
                RichAgentEvent::ToolExecutionEnd {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "bash".to_string(),
                    result: lifecycle_result,
                    is_error: false,
                },
            ),
        ];
        let mut transcript = events
            .into_iter()
            .map(|(case_name, event)| {
                let line = serialize_rpc_prompt_event(event).unwrap();
                let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                serde_json::json!({
                    "case": case_name,
                    "record": rpc_event_signature(&value),
                })
            })
            .collect::<Vec<_>>();
        for (case_name, value) in [
            (
                "agent_settled",
                serde_json::json!({"type": "agent_settled"}),
            ),
            (
                "queue_update",
                serde_json::json!({
                    "type": "queue_update",
                    "steering": ["steer"],
                    "followUp": ["follow"],
                }),
            ),
            (
                "compaction_start",
                serde_json::json!({"type": "compaction_start", "reason": "manual"}),
            ),
            (
                "compaction_end",
                serde_json::json!({
                    "type": "compaction_end",
                    "reason": "manual",
                    "result": {
                        "summary": "summary",
                        "firstKeptEntryId": "entry-1",
                        "tokensBefore": 12,
                        "estimatedTokensAfter": 4,
                    },
                    "aborted": false,
                    "willRetry": false,
                }),
            ),
            (
                "session_info_changed",
                serde_json::json!({"type": "session_info_changed", "name": "session"}),
            ),
            (
                "thinking_level_changed",
                serde_json::json!({"type": "thinking_level_changed", "level": "high"}),
            ),
            (
                "summarization_retry_scheduled",
                serde_json::json!({
                    "type": "summarization_retry_scheduled",
                    "attempt": 1,
                    "maxAttempts": 2,
                    "delayMs": 25,
                    "errorMessage": "retry",
                }),
            ),
            (
                "summarization_retry_attempt_start",
                serde_json::json!({
                    "type": "summarization_retry_attempt_start",
                    "source": "compaction",
                    "reason": "manual",
                }),
            ),
            (
                "summarization_retry_finished",
                serde_json::json!({"type": "summarization_retry_finished"}),
            ),
        ] {
            transcript.push(serde_json::json!({
                "case": case_name,
                "record": rpc_event_signature(&value),
            }));
        }
        let bash_update: serde_json::Value =
            serde_json::from_str(serialize_rpc_bash_update(Some("bash-1"), "chunk").trim())
                .unwrap();
        transcript.push(serde_json::json!({
            "case": "bash_execution_update",
            "record": rpc_event_signature(&bash_update),
        }));
        let expected: Vec<serde_json::Value> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rpc/event_transcript.json"
        )))
        .unwrap();
        assert_eq!(transcript, expected);
    }

    #[tokio::test]
    async fn tool_terminate_hint_reaches_rpc_and_session_contracts() {
        let mut lifecycle_result = pi_agent::tools::AgentToolResult::output("done");
        lifecycle_result.terminate = true;
        let end = RichAgentEvent::ToolExecutionEnd {
            tool_call_id: "call-terminate".to_string(),
            tool_name: "stop".to_string(),
            result: lifecycle_result,
            is_error: false,
        };
        let mut pending_terminations = HashSet::new();
        let mut persisted = Vec::new();
        capture_persisted_rpc_event(&end, &mut pending_terminations, &mut persisted);

        let tool_message = pi_agent::types::AgentMessage::Core(Message::ToolResult(
            pi_ai::types::ToolResultMessage::text("call-terminate", "stop", "done", false),
        ));
        capture_persisted_rpc_event(
            &RichAgentEvent::MessageEnd {
                message: tool_message,
            },
            &mut pending_terminations,
            &mut persisted,
        );
        assert_eq!(persisted.len(), 1);
        assert!(persisted[0].terminate);

        let event_json = serialize_rpc_prompt_event(end).unwrap();
        let event_json: serde_json::Value = serde_json::from_str(event_json.trim()).unwrap();
        assert_eq!(event_json["result"]["terminate"], true);

        let mut runtime = runtime_for_test().await;
        runtime
            .persist_messages_with_termination(&persisted)
            .await
            .unwrap();
        let entries = runtime.get_entries().await.unwrap();
        assert!(entries.iter().any(|entry| {
            matches!(
                entry,
                pi_agent::session::types::Entry::Message {
                    message: pi_agent::types::AgentMessage::Core(Message::ToolResult(result)),
                    terminate: Some(true),
                    ..
                } if result.tool_call_id() == "call-terminate"
            )
        }));
    }

    #[tokio::test]
    async fn rpc_command_golden_transcript_matches_fixture() {
        let mut runtime = runtime_for_test().await;
        let mut transcript = Vec::new();
        capture_rpc_command(
            &mut runtime,
            "state.initial",
            serde_json::json!({"id":"state-0","type":"get_state"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "prompt.success",
            serde_json::json!({"id":"prompt-1","type":"prompt","message":"golden"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "steer.success",
            serde_json::json!({"id":"steer-1","type":"steer","message":"interrupt"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "follow_up.success",
            serde_json::json!({"id":"follow-1","type":"follow_up","message":"continue"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_steering_mode.success",
            serde_json::json!({"id":"steering-mode","type":"set_steering_mode","mode":"one-at-a-time"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_follow_up_mode.success",
            serde_json::json!({"id":"follow-mode","type":"set_follow_up_mode","mode":"one-at-a-time"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "state.queued",
            serde_json::json!({"id":"state-1","type":"get_state"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "abort.success",
            serde_json::json!({"id":"abort-1","type":"abort"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "compact.success",
            serde_json::json!({"id":"compact-1","type":"compact"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_auto_compaction.success",
            serde_json::json!({"id":"auto-compact","type":"set_auto_compaction","enabled":false}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_auto_retry.success",
            serde_json::json!({"id":"auto-retry","type":"set_auto_retry","enabled":false}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "abort_retry.success",
            serde_json::json!({"id":"abort-retry","type":"abort_retry"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "bash.success",
            serde_json::json!({"id":"bash-1","type":"bash","command":"printf golden"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "abort_bash.success",
            serde_json::json!({"id":"abort-bash","type":"abort_bash"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_available_models.success",
            serde_json::json!({"id":"models","type":"get_available_models"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_model.success",
            serde_json::json!({"id":"set-model","type":"set_model","provider":"google","modelId":"gemini-2.5-flash"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_model.unknown",
            serde_json::json!({"id":"set-model-bad","type":"set_model","provider":"nope","modelId":"missing"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "cycle_model.success",
            serde_json::json!({"id":"cycle-model","type":"cycle_model"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_thinking_level.success",
            serde_json::json!({"id":"thinking","type":"set_thinking_level","level":"high"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "cycle_thinking_level.success",
            serde_json::json!({"id":"cycle-thinking","type":"cycle_thinking_level"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_available_thinking_levels.success",
            serde_json::json!({"id":"thinking-levels","type":"get_available_thinking_levels"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_session_stats.success",
            serde_json::json!({"id":"stats","type":"get_session_stats"}),
            &mut transcript,
        )
        .await;
        let original_session_path = runtime.session_path.clone().unwrap();
        let entries = runtime.get_entries().await.unwrap();
        let first_message_id = entries
            .iter()
            .find(|entry| entry.as_message().is_some())
            .map(|entry| entry.id().to_string())
            .unwrap();
        let export_path = std::env::temp_dir()
            .join(format!("pi-rpc-golden-{}.html", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        capture_rpc_command(
            &mut runtime,
            "export_html.success",
            serde_json::json!({"id":"export","type":"export_html","outputPath":export_path}),
            &mut transcript,
        )
        .await;
        let _ = std::fs::remove_file(&export_path);
        capture_rpc_command(
            &mut runtime,
            "get_fork_messages.success",
            serde_json::json!({"id":"fork-messages","type":"get_fork_messages"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_entries.success",
            serde_json::json!({"id":"entries","type":"get_entries"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_tree.success",
            serde_json::json!({"id":"tree","type":"get_tree"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_last_assistant_text.success",
            serde_json::json!({"id":"last","type":"get_last_assistant_text"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_messages.success",
            serde_json::json!({"id":"messages","type":"get_messages"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "set_session_name.success",
            serde_json::json!({"id":"name","type":"set_session_name","name":"golden session"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "get_commands.success",
            serde_json::json!({"id":"commands","type":"get_commands"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "fork.success",
            serde_json::json!({"id":"fork","type":"fork","entryId":first_message_id}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "switch_session.success",
            serde_json::json!({"id":"switch","type":"switch_session","sessionPath":original_session_path}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "clone.failure",
            serde_json::json!({"id":"clone","type":"clone"}),
            &mut transcript,
        )
        .await;
        capture_rpc_command(
            &mut runtime,
            "new_session.success",
            serde_json::json!({"id":"new","type":"new_session"}),
            &mut transcript,
        )
        .await;

        for (case_name, input) in [
            (
                "prompt.missing_message",
                serde_json::json!({"id":"e1","type":"prompt"}),
            ),
            (
                "steer.missing_message",
                serde_json::json!({"id":"e2","type":"steer"}),
            ),
            (
                "follow_up.missing_message",
                serde_json::json!({"id":"e3","type":"follow_up"}),
            ),
            (
                "set_model.missing_provider",
                serde_json::json!({"id":"e4","type":"set_model","modelId":"x"}),
            ),
            (
                "set_model.missing_model",
                serde_json::json!({"id":"e5","type":"set_model","provider":"x"}),
            ),
            (
                "bash.missing_command",
                serde_json::json!({"id":"e6","type":"bash"}),
            ),
            (
                "switch_session.missing_path",
                serde_json::json!({"id":"e7","type":"switch_session"}),
            ),
            (
                "fork.missing_entry",
                serde_json::json!({"id":"e8","type":"fork"}),
            ),
            (
                "set_session_name.empty",
                serde_json::json!({"id":"e9","type":"set_session_name","name":"  "}),
            ),
            (
                "set_steering_mode.invalid",
                serde_json::json!({"id":"e10","type":"set_steering_mode","mode":"invalid"}),
            ),
            (
                "set_follow_up_mode.invalid",
                serde_json::json!({"id":"e11","type":"set_follow_up_mode","mode":"invalid"}),
            ),
            (
                "set_model.unknown_error",
                serde_json::json!({"id":"e12","type":"set_model","provider":"nope","modelId":"missing"}),
            ),
            (
                "get_entries.unknown_since",
                serde_json::json!({"id":"e13","type":"get_entries","since":"missing-entry"}),
            ),
            (
                "switch_session.invalid_path",
                serde_json::json!({"id":"e14","type":"switch_session","sessionPath":"/missing/session.jsonl"}),
            ),
            (
                "unknown.command",
                serde_json::json!({"id":"e15","type":"not_a_command"}),
            ),
        ] {
            capture_rpc_command(&mut runtime, case_name, input, &mut transcript).await;
        }
        let mut in_memory = runtime_for_test().await;
        in_memory.session_path = None;
        capture_rpc_command(
            &mut in_memory,
            "export_html.in_memory",
            serde_json::json!({"id":"e16","type":"export_html"}),
            &mut transcript,
        )
        .await;
        let mut empty = runtime_for_test().await;
        capture_rpc_command(
            &mut empty,
            "clone.empty",
            serde_json::json!({"id":"e17","type":"clone"}),
            &mut transcript,
        )
        .await;

        let expected: Vec<serde_json::Value> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rpc/command_transcript.json"
        )))
        .unwrap();
        assert_eq!(transcript, expected);
    }
}
