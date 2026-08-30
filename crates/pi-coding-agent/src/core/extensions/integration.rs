//! Shared integration between the extension runner and coding-agent modes.
//!
//! The loader/runner deliberately owns extension protocol details.  This
//! module owns the small adapter needed by the agent loop: a host-action state
//! object, extension-tool conversion, and mode-scoped loading policy.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pi_agent::tools::{
    AgentTool, AgentToolResult, ToolExecuteFn, ToolExecutionMode as AgentToolExecutionMode,
    ToolPrepareArgumentsFn as AgentToolPrepareArgumentsFn, ToolUpdateCallback,
};
use pi_ai::auth::{ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthResult, ModelAuth};
use pi_ai::event_stream::{create_error_stream, AssistantMessageEventStream};
use pi_ai::model::Model;
use pi_ai::models::{
    create_provider, CreateProviderOptions, Models, ProviderApiSpec, ProviderStreams,
};
use pi_ai::types::{
    json_tool, AssistantMessage, AssistantMessageEvent, ConstrainedSampling, ContentBlock, Context,
    DoneReason, ErrorReason, SimpleStreamOptions, StopReason, StreamOptions, StrictPreference,
};
use serde_json::{json, Value};

use crate::args::{Args, ExtensionFlagValue};
use crate::core::settings::SettingsManager;

use super::loader::{discover_and_load_extensions, load_extensions_with_host_actions};
use super::runner::{ExtensionRunner, ResourceDiscovery};
use super::types::{
    ExtensionHostAction, ExtensionHostActionOutcome, ExtensionHostActions, ExtensionLoadError,
    ExtensionUiBroker, ExtensionUiDiagnostic, ExtensionUiRequestSink,
    ExtensionUiResponseDisposition, LifecycleCompletionSink, PendingHostAction,
    PendingNativeProviderRegistration, RegisteredTool, RequestedHostChanges, TerminalInputDispatch,
    ToolExecutionMode, ToolUpdateFn,
};

fn native_context_value(context: &Context) -> Value {
    json!({
        "systemPrompt": context.system_prompt,
        "messages": context.messages,
        "tools": context.tools,
    })
}

/// Convert the parser's extension-facing flag tokens into the runtime map
/// consumed by native extension contexts.
///
/// This is deliberately only a value conversion: it does not load extensions
/// or emit lifecycle events. The default mode loader calls it once before
/// `session_start`, while reload/session-replacement callers pass their
/// already-live flag snapshot through the explicit `*_and_flags` API.
pub fn parsed_extension_flag_values(args: &Args) -> Option<BTreeMap<String, Value>> {
    let mut values = BTreeMap::new();
    for (name, value) in &args.extension_flag_values {
        let value = match value {
            ExtensionFlagValue::Boolean(value) => Value::Bool(*value),
            ExtensionFlagValue::String(value) => Value::String(value.clone()),
        };
        values.insert(name.clone(), value);
    }
    (!values.is_empty()).then_some(values)
}

fn native_stream_options_value(options: Option<&StreamOptions>) -> Value {
    let Some(options) = options else {
        return Value::Null;
    };
    json!({
        "apiKey": options.base.api_key,
        "headers": options.base.headers,
        "timeoutMs": options.base.timeout_ms,
        "maxRetries": options.base.max_retries,
        "maxRetryDelayMs": options.base.max_retry_delay_ms,
        "temperature": options.temperature,
        "samplingParams": options.sampling_params,
        "maxTokens": options.max_tokens,
        "transport": options.transport,
        "cacheRetention": options.cache_retention,
        "sessionId": options.session_id,
        "websocketConnectTimeoutMs": options.websocket_connect_timeout_ms,
        "metadata": options.metadata,
    })
}

fn native_simple_options_value(options: Option<&SimpleStreamOptions>) -> Value {
    let Some(options) = options else {
        return Value::Null;
    };
    let mut value = native_stream_options_value(Some(&options.base));
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "reasoning".to_string(),
            options
                .reasoning
                .map(|reasoning| Value::String(format!("{reasoning:?}")))
                .unwrap_or(Value::Null),
        );
        object.insert(
            "thinkingBudgets".to_string(),
            serde_json::to_value(&options.thinking_budgets).unwrap_or(Value::Null),
        );
        object.insert(
            "toolChoice".to_string(),
            serde_json::to_value(options.tool_choice).unwrap_or(Value::Null),
        );
        object.insert(
            "deferred".to_string(),
            match options.deferred {
                None => Value::Null,
                Some(pi_ai::types::DeferredOption::Bool(value)) => Value::Bool(value),
                Some(pi_ai::types::DeferredOption::Window(window)) => Value::String(
                    match window {
                        pi_ai::types::DeferredWindow::M15 => "15m",
                        pi_ai::types::DeferredWindow::H1 => "1h",
                        pi_ai::types::DeferredWindow::H24 => "24h",
                    }
                    .to_string(),
                ),
            },
        );
    }
    value
}

fn default_native_message(model: &Model) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model(&model.api, &model.provider, &model.id);
    message
}

fn parse_stop_reason(value: Option<&Value>) -> Option<StopReason> {
    match value.and_then(Value::as_str) {
        Some("pending") => Some(StopReason::Pending),
        Some("stop") => Some(StopReason::Stop),
        Some("length") => Some(StopReason::Length),
        Some("tool_use" | "toolUse") => Some(StopReason::ToolUse),
        Some("error") => Some(StopReason::Error),
        Some("aborted") => Some(StopReason::Aborted),
        Some("deferred") => Some(StopReason::Deferred),
        _ => None,
    }
}

fn parse_assistant_message(value: Option<&Value>, model: &Model) -> AssistantMessage {
    let Some(value) = value else {
        return default_native_message(model);
    };
    if let Some(error) = value.as_str() {
        let mut message = default_native_message(model);
        message.set_error_message(error);
        return message;
    }
    if let Ok(message) = serde_json::from_value::<AssistantMessage>(value.clone()) {
        return message;
    }
    let mut message = default_native_message(model);
    if let Some(content) = value.get("content") {
        if let Ok(content) = serde_json::from_value::<Vec<ContentBlock>>(content.clone()) {
            message.set_content(content);
        }
    }
    if let Some(reason) =
        parse_stop_reason(value.get("stopReason").or_else(|| value.get("stop_reason")))
    {
        message.set_stop_reason(reason);
    }
    if let Some(error) = value
        .get("errorMessage")
        .or_else(|| value.get("error_message"))
        .and_then(Value::as_str)
    {
        message.set_error_message(error);
    }
    if let Some(usage) = value.get("usage") {
        if let Ok(usage) = serde_json::from_value(usage.clone()) {
            message.set_usage(usage);
        }
    }
    message
}

fn parse_done_reason(value: Option<&Value>) -> DoneReason {
    match value.and_then(Value::as_str).unwrap_or("stop") {
        "length" => DoneReason::Length,
        "tool_use" | "toolUse" => DoneReason::ToolUse,
        "deferred" => DoneReason::Deferred,
        _ => DoneReason::Stop,
    }
}

fn parse_error_reason(value: Option<&Value>) -> ErrorReason {
    match value.and_then(Value::as_str).unwrap_or("error") {
        "aborted" => ErrorReason::Aborted,
        _ => ErrorReason::Error,
    }
}

fn parse_native_event(value: &Value, model: &Model) -> Result<AssistantMessageEvent, String> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "native provider event is missing type".to_string())?;
    let partial = || parse_assistant_message(value.get("partial"), model);
    let index = || {
        value
            .get("contentIndex")
            .or_else(|| value.get("content_index"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize
    };
    match event_type {
        "start" => Ok(AssistantMessageEvent::Start { partial: partial() }),
        "text_start" => Ok(AssistantMessageEvent::TextStart {
            content_index: index(),
            partial: partial(),
        }),
        "text_delta" => Ok(AssistantMessageEvent::TextDelta {
            content_index: index(),
            delta: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            partial: partial(),
        }),
        "text_end" => Ok(AssistantMessageEvent::TextEnd {
            content_index: index(),
            content: value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            partial: partial(),
        }),
        "thinking_start" => Ok(AssistantMessageEvent::ThinkingStart {
            content_index: index(),
            partial: partial(),
        }),
        "thinking_delta" => Ok(AssistantMessageEvent::ThinkingDelta {
            content_index: index(),
            delta: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            partial: partial(),
        }),
        "thinking_end" => Ok(AssistantMessageEvent::ThinkingEnd {
            content_index: index(),
            content: value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            partial: partial(),
        }),
        "toolcall_start" | "tool_call_start" => Ok(AssistantMessageEvent::ToolCallStart {
            content_index: index(),
            partial: partial(),
        }),
        "toolcall_delta" | "tool_call_delta" => Ok(AssistantMessageEvent::ToolCallDelta {
            content_index: index(),
            delta: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            partial: partial(),
        }),
        "toolcall_end" | "tool_call_end" => {
            let tool_call = value
                .get("toolCall")
                .or_else(|| value.get("tool_call"))
                .cloned()
                .ok_or_else(|| "native tool_call_end event is missing toolCall".to_string())?;
            let tool_call = serde_json::from_value(tool_call)
                .map_err(|error| format!("invalid native tool call: {error}"))?;
            Ok(AssistantMessageEvent::ToolCallEnd {
                content_index: index(),
                tool_call,
                partial: partial(),
            })
        }
        "done" => Ok(AssistantMessageEvent::Done {
            reason: parse_done_reason(value.get("reason")),
            message: parse_assistant_message(value.get("message"), model),
        }),
        "error" => Ok(AssistantMessageEvent::Error {
            reason: parse_error_reason(value.get("reason")),
            error_message: parse_assistant_message(
                value
                    .get("errorMessage")
                    .or_else(|| value.get("error"))
                    .or_else(|| value.get("message")),
                model,
            ),
        }),
        other => Err(format!("unsupported native provider event type: {other}")),
    }
}

fn native_events_to_stream(model: &Model, events: Vec<Value>) -> AssistantMessageEventStream {
    let mut stream = AssistantMessageEventStream::new();
    let mut terminal = false;
    for event in events {
        match parse_native_event(&event, model) {
            Ok(parsed) => {
                terminal = matches!(
                    parsed,
                    AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
                );
                stream.push(parsed);
            }
            Err(error) => {
                return create_error_stream(&model.api, &model.provider, &model.id, error);
            }
        }
    }
    if !terminal {
        return create_error_stream(
            &model.api,
            &model.provider,
            &model.id,
            "native provider callback ended without a terminal event".to_string(),
        );
    }
    stream
}

fn native_provider_streams(
    registration: &PendingNativeProviderRegistration,
) -> Result<ProviderStreams, String> {
    let full = registration
        .callbacks
        .get("stream")
        .cloned()
        .or_else(|| registration.callbacks.get("streamSimple").cloned())
        .ok_or_else(|| {
            format!(
                "native provider {:?} has no stream callback",
                registration.provider
            )
        })?;
    let simple = registration
        .callbacks
        .get("streamSimple")
        .cloned()
        .or_else(|| registration.callbacks.get("stream").cloned())
        .ok_or_else(|| {
            format!(
                "native provider {:?} has no stream callback",
                registration.provider
            )
        })?;
    let stream = Arc::new(
        move |model: &Model, context: &Context, options: Option<&StreamOptions>| {
            let events = full(
                serde_json::to_value(model).unwrap_or(Value::Null),
                native_context_value(context),
                native_stream_options_value(options),
            );
            match events {
                Ok(events) => native_events_to_stream(model, events),
                Err(error) => create_error_stream(&model.api, &model.provider, &model.id, error),
            }
        },
    );
    let stream_simple = Arc::new(
        move |model: &Model, context: &Context, options: Option<&SimpleStreamOptions>| {
            let events = simple(
                serde_json::to_value(model).unwrap_or(Value::Null),
                native_context_value(context),
                native_simple_options_value(options),
            );
            match events {
                Ok(events) => native_events_to_stream(model, events),
                Err(error) => create_error_stream(&model.api, &model.provider, &model.id, error),
            }
        },
    );
    Ok(ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    })
}

struct NativeProviderAuth;

impl ApiKeyAuth for NativeProviderAuth {
    fn name(&self) -> &str {
        "extension provider"
    }

    fn check(
        &self,
        _ctx: &AuthContext,
        _credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthCheck> {
        Some(AuthCheck {
            source: Some("extension".to_string()),
            auth_type: "api_key",
        })
    }

    fn resolve(
        &self,
        _ctx: &AuthContext,
        _credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult> {
        Some(AuthResult {
            auth: ModelAuth::default(),
            env: None,
            source: Some("extension".to_string()),
        })
    }
}

fn native_models(registration: &PendingNativeProviderRegistration) -> Result<Vec<Model>, String> {
    let definition = registration
        .definition
        .as_object()
        .ok_or_else(|| "native provider definition must be an object".to_string())?;
    let default_api = definition
        .get("api")
        .and_then(Value::as_str)
        .unwrap_or("openai-completions");
    let default_base_url = definition
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw_models = definition
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("native provider {:?} has no models", registration.provider))?;
    raw_models
        .iter()
        .map(|raw| {
            let mut object = raw
                .as_object()
                .cloned()
                .ok_or_else(|| "native provider model must be an object".to_string())?;
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "native provider model is missing id".to_string())?;
            object
                .entry("name".to_string())
                .or_insert_with(|| Value::String(id.clone()));
            object
                .entry("api".to_string())
                .or_insert_with(|| Value::String(default_api.to_string()));
            object
                .entry("provider".to_string())
                .or_insert_with(|| Value::String(registration.provider.clone()));
            object
                .entry("baseUrl".to_string())
                .or_insert_with(|| Value::String(default_base_url.to_string()));
            object
                .entry("reasoning".to_string())
                .or_insert(Value::Bool(false));
            object
                .entry("input".to_string())
                .or_insert_with(|| json!(["text"]));
            object.entry("cost".to_string()).or_insert_with(
                || json!({"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0}),
            );
            object
                .entry("contextWindow".to_string())
                .or_insert(Value::from(128_000_u64));
            object
                .entry("maxTokens".to_string())
                .or_insert(Value::from(16_384_u64));
            serde_json::from_value(Value::Object(object))
                .map_err(|error| format!("invalid native provider model {id:?}: {error}"))
        })
        .collect()
}

/// Register an externally-defined native provider in the Rust Models facade.
/// The bridge owns callback execution; this helper owns typed model and event
/// conversion so mode startup can bind the provider without a second runtime.
pub fn register_native_provider(
    models: &Models,
    registration: &PendingNativeProviderRegistration,
) -> Result<(), String> {
    let definition = registration
        .definition
        .as_object()
        .ok_or_else(|| "native provider definition must be an object".to_string())?;
    let provider = create_provider(CreateProviderOptions {
        id: registration.provider.clone(),
        name: definition
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        base_url: definition
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_string),
        headers: None,
        auth: pi_ai::auth::ProviderAuth {
            api_key: Some(Arc::new(NativeProviderAuth)),
            oauth: None,
        },
        models: native_models(registration)?,
        api: ProviderApiSpec::Single(native_provider_streams(registration)?),
        filter_models: None,
    });
    models.set_provider(provider);
    Ok(())
}

/// Register every native provider queued by a loaded extension set.
pub fn register_loaded_native_providers(
    models: &Models,
    loaded: &LoadedExtensions,
) -> Result<usize, String> {
    let registrations = loaded
        .runtime
        .lock()
        .map_err(|_| "Extension runtime lock poisoned".to_string())?
        .pending_native_provider_registrations
        .clone();
    for registration in &registrations {
        register_native_provider(models, registration)?;
    }
    Ok(registrations.len())
}

#[derive(Debug)]
struct ExtensionHostStateInner {
    session_name: Option<String>,
    active_tools: Vec<String>,
    all_tools: Vec<Value>,
    commands: Vec<Value>,
    thinking_level: String,
    model: Option<Value>,
    scoped_models: Vec<Value>,
    is_idle: bool,
    is_project_trusted: bool,
    signal: Option<Arc<AtomicBool>>,
    tool_signals: BTreeMap<String, Arc<AtomicBool>>,
    has_pending_messages: bool,
    context_usage: Option<Value>,
    system_prompt: String,
    system_prompt_options: Value,
    requested_model: Option<Value>,
    requested_model_request: Option<PendingHostAction>,
    requested_active_tools: Option<Vec<String>>,
    pending_messages: Vec<Value>,
    pending_entries: Vec<Value>,
    pending_actions: Vec<PendingHostAction>,
    pending_tool_updates: Vec<(Option<String>, Value)>,
    labels: Vec<Value>,
}

impl Default for ExtensionHostStateInner {
    fn default() -> Self {
        Self {
            session_name: None,
            active_tools: Vec::new(),
            all_tools: Vec::new(),
            commands: Vec::new(),
            thinking_level: "medium".to_string(),
            model: None,
            scoped_models: Vec::new(),
            is_idle: true,
            is_project_trusted: true,
            signal: None,
            tool_signals: BTreeMap::new(),
            has_pending_messages: false,
            context_usage: None,
            system_prompt: String::new(),
            system_prompt_options: json!({}),
            requested_model: None,
            requested_model_request: None,
            requested_active_tools: None,
            pending_messages: Vec::new(),
            pending_entries: Vec::new(),
            pending_actions: Vec::new(),
            pending_tool_updates: Vec::new(),
            labels: Vec::new(),
        }
    }
}

/// Host-owned state shared by an extension bridge and the active mode.
///
/// The mode can consume the queued message/entry/session requests after a
/// callback, while synchronous getters are served from the same snapshot that
/// the bridge receives for every callback. Session replacement is deliberately
/// queued at this non-reentrant boundary; a cross-process callback cannot
/// provide upstream's callback-scoped `withSession` continuation.
#[derive(Clone)]
pub struct ExtensionHostState {
    inner: Arc<Mutex<ExtensionHostStateInner>>,
    idle_wakeup: Arc<Condvar>,
    lifecycle_completion_sink: Arc<Mutex<Option<LifecycleCompletionSink>>>,
    ui_broker: ExtensionUiBroker,
}

impl std::fmt::Debug for ExtensionHostState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has_lifecycle_completion_sink = self
            .lifecycle_completion_sink
            .lock()
            .map(|sink| sink.is_some())
            .unwrap_or(false);
        formatter
            .debug_struct("ExtensionHostState")
            .field("inner", &self.inner)
            .field(
                "has_lifecycle_completion_sink",
                &has_lifecycle_completion_sink,
            )
            .field("ui_broker", &self.ui_broker)
            .finish()
    }
}

impl Default for ExtensionHostState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExtensionHostStateInner::default())),
            idle_wakeup: Arc::new(Condvar::new()),
            lifecycle_completion_sink: Arc::new(Mutex::new(None)),
            ui_broker: ExtensionUiBroker::new(),
        }
    }
}

impl ExtensionHostState {
    pub fn new(session_name: Option<String>, thinking_level: impl Into<String>) -> Self {
        let state = Self::default();
        if let Ok(mut inner) = state.inner.lock() {
            inner.session_name = session_name;
            inner.thinking_level = thinking_level.into();
        }
        state
    }

    /// Replace the synchronous tool/command catalog visible to extension
    /// callbacks.  `active_tools` is intentionally explicit because modes
    /// may disable all tools or only the built-ins.
    pub fn set_catalog(
        &self,
        active_tools: Vec<String>,
        all_tools: Vec<Value>,
        commands: Vec<Value>,
    ) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.active_tools = active_tools;
            inner.all_tools = all_tools;
            inner.commands = commands;
        }
    }

    pub fn active_tools(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|inner| inner.active_tools.clone())
            .unwrap_or_default()
    }

    /// Return the correlated UI broker shared by native extension contexts
    /// and the active mode. Clones refer to the same pending-request state.
    pub fn ui_broker(&self) -> ExtensionUiBroker {
        self.ui_broker.clone()
    }

    pub fn set_ui_enabled(&self, enabled: bool) {
        self.ui_broker.set_enabled(enabled);
    }

    pub fn set_ui_request_sink(&self, sink: ExtensionUiRequestSink) {
        self.ui_broker.set_request_sink(sink);
    }

    pub fn drain_ui_requests(&self) -> Vec<Value> {
        self.ui_broker.drain_requests()
    }

    pub fn handle_ui_response(&self, response: &Value) -> ExtensionUiResponseDisposition {
        self.ui_broker.handle_response(response)
    }

    /// Deliver raw terminal bytes to the listeners registered by native
    /// extension UI contexts. The broker owns ordering, transformation, and
    /// consume semantics, while the mode owns what it does with the returned
    /// payload.
    pub fn dispatch_terminal_input(&self, input: &str) -> Result<TerminalInputDispatch, String> {
        self.ui_broker.dispatch_terminal_input(input)
    }

    pub fn ui_state_snapshot(&self) -> Value {
        self.ui_broker.ui_state_snapshot()
    }

    pub fn set_ui_themes(&self, themes: Vec<Value>) {
        self.ui_broker.set_themes(themes);
    }

    pub fn set_current_ui_theme(&self, theme: Option<Value>) {
        self.ui_broker.set_current_theme(theme);
    }

    pub fn drain_ui_diagnostics(&self) -> Vec<ExtensionUiDiagnostic> {
        self.ui_broker.drain_diagnostics()
    }

    /// Return live extension status rows for the interactive footer. The
    /// broker retains these separately from general UI requests because the
    /// interactive terminal owns their rendering.
    pub fn extension_statuses(&self) -> Vec<(String, String)> {
        self.ui_broker.extension_statuses()
    }

    /// Wake the interactive owner when a native extension changes a footer
    /// status, including updates made from an extension worker thread.
    pub fn extension_status_wakeup(&self) -> Arc<tokio::sync::Notify> {
        self.ui_broker.status_wakeup()
    }

    pub fn cancel_ui_requests(&self) {
        self.ui_broker.cancel_all();
    }

    /// Return the latest model request without consuming it. The mode can use
    /// this to inspect work before deciding when to apply it.
    pub fn requested_model(&self) -> Option<Value> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.requested_model.clone())
    }

    /// Return the latest model request, including bridge continuation data,
    /// without consuming it.
    pub fn requested_model_change(&self) -> Option<PendingHostAction> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.requested_model_request.clone())
    }

    /// Return the latest active-tool request without consuming it.
    pub fn requested_active_tools(&self) -> Option<Vec<String>> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.requested_active_tools.clone())
    }

    pub fn set_model(&self, model: Option<Value>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.model = model;
        }
    }

    pub fn set_thinking_level(&self, thinking_level: impl Into<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.thinking_level = thinking_level.into();
        }
    }

    pub fn set_scoped_models(&self, scoped_models: Vec<Value>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.scoped_models = scoped_models;
        }
    }

    pub fn set_idle(&self, is_idle: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.is_idle = is_idle;
            self.idle_wakeup.notify_all();
        }
    }

    /// Maximum time the bridge-facing `waitForIdle` action may block.
    pub const MAX_WAIT_FOR_IDLE: Duration = Duration::from_secs(60);

    /// Wait until the host reports idle, using the bounded production timeout.
    pub fn wait_for_idle(&self) -> Result<(), String> {
        self.wait_for_idle_timeout(Self::MAX_WAIT_FOR_IDLE)
    }

    /// Wait until the host reports idle or the supplied bound expires.
    ///
    /// The condition is checked while holding the same mutex that `set_idle`
    /// updates, and `set_idle` wakes all waiters. This avoids polling and also
    /// avoids a lost wakeup between the check and the sleep.
    pub fn wait_for_idle_timeout(&self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Extension host state lock poisoned".to_string())?;
        while !inner.is_idle {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    format!("Timed out waiting for extension host to become idle after {timeout:?}")
                })?;
            let (next, wait_result) = self
                .idle_wakeup
                .wait_timeout(inner, remaining)
                .map_err(|_| "Extension host state lock poisoned".to_string())?;
            inner = next;
            if wait_result.timed_out() && !inner.is_idle {
                return Err(format!(
                    "Timed out waiting for extension host to become idle after {timeout:?}"
                ));
            }
        }
        Ok(())
    }

    pub fn set_project_trusted(&self, is_project_trusted: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.is_project_trusted = is_project_trusted;
        }
    }

    /// Bind the signal for the currently executing agent tool. The bridge
    /// receives only an `aborted` snapshot; the shared flag still lets an
    /// external `ctx.abort()` action cancel the Rust-side operation.
    pub fn set_signal(&self, signal: Option<Arc<AtomicBool>>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.signal = signal;
        }
    }

    pub fn set_has_pending_messages(&self, has_pending_messages: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.has_pending_messages = has_pending_messages;
        }
    }

    pub fn set_context_usage(&self, context_usage: Option<Value>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.context_usage = context_usage;
        }
    }

    pub fn set_system_prompt(&self, system_prompt: impl Into<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.system_prompt = system_prompt.into();
        }
    }

    pub fn set_system_prompt_options(&self, system_prompt_options: Value) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.system_prompt_options = system_prompt_options;
        }
    }

    /// Start a tool callback boundary and clear updates left by a prior call.
    pub fn begin_tool_execution(&self, tool_call_id: &str, signal: Option<Arc<AtomicBool>>) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(signal) = signal {
                inner.tool_signals.insert(tool_call_id.to_string(), signal);
            } else {
                inner.tool_signals.remove(tool_call_id);
            }
            inner
                .pending_tool_updates
                .retain(|(call_id, _)| call_id.as_deref() != Some(tool_call_id));
        }
    }

    /// End a tool callback boundary, returning bridge updates in protocol
    /// arrival order. Clearing the signal prevents it leaking into the next
    /// handler or tool request.
    pub fn end_tool_execution(&self, tool_call_id: &str) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| {
                inner.tool_signals.remove(tool_call_id);
                let mut updates = Vec::new();
                let mut remaining = Vec::new();
                for (call_id, update) in std::mem::take(&mut inner.pending_tool_updates) {
                    if call_id.as_deref() == Some(tool_call_id) {
                        updates.push(update);
                    } else {
                        remaining.push((call_id, update));
                    }
                }
                inner.pending_tool_updates = remaining;
                updates
            })
            .unwrap_or_default()
    }

    /// Drain asynchronous messages requested by extensions.  The current
    /// mode owns delivery semantics; retaining them here prevents a bridge
    /// callback from recursively entering the agent loop.
    pub fn drain_pending_messages(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.pending_messages))
            .unwrap_or_default()
    }

    pub fn drain_pending_entries(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.pending_entries))
            .unwrap_or_default()
    }

    /// Consume the latest model request, if any.
    pub fn drain_requested_model(&self) -> Option<Value> {
        self.inner.lock().ok().and_then(|mut inner| {
            inner.requested_model_request.take();
            inner.requested_model.take()
        })
    }

    /// Consume the latest model request with its typed action and optional
    /// bridge continuation metadata intact.
    pub fn drain_requested_model_change(&self) -> Option<PendingHostAction> {
        self.inner.lock().ok().and_then(|mut inner| {
            let request = inner.requested_model_request.take();
            inner.requested_model.take();
            request
        })
    }

    /// Consume the latest active-tool request, if any.
    pub fn drain_requested_active_tools(&self) -> Option<Vec<String>> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut inner| inner.requested_active_tools.take())
    }

    /// Atomically consume all model and active-tool requests currently
    /// waiting for mode application.
    pub fn drain_requested_changes(&self) -> RequestedHostChanges {
        self.inner
            .lock()
            .map(|mut inner| RequestedHostChanges {
                model: inner.requested_model.take(),
                model_request: inner.requested_model_request.take(),
                active_tools: inner.requested_active_tools.take(),
            })
            .unwrap_or_default()
    }

    pub fn drain_pending_actions(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| {
                std::mem::take(&mut inner.pending_actions)
                    .into_iter()
                    .map(|action| action.payload)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drain only session/lifecycle actions so message, abort, shutdown, and
    /// compaction requests remain owned by the mode-specific dispatcher.
    /// Lifecycle actions are consumed at a non-reentrant mode boundary after
    /// the callback that enqueued them has returned.
    pub fn drain_pending_lifecycle_actions(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| {
                let mut lifecycle = Vec::new();
                let mut remaining = Vec::new();
                for action in std::mem::take(&mut inner.pending_actions) {
                    if action.is_lifecycle() {
                        lifecycle.push(action.payload);
                    } else {
                        remaining.push(action);
                    }
                }
                inner.pending_actions = remaining;
                lifecycle
            })
            .unwrap_or_default()
    }

    /// Drain lifecycle requests with their typed action and original bridge
    /// arguments intact for the completion-aware loader bridge.
    pub fn drain_pending_lifecycle_action_metadata(&self) -> Vec<PendingHostAction> {
        self.inner
            .lock()
            .map(|mut inner| {
                let mut lifecycle = Vec::new();
                let mut remaining = Vec::new();
                for action in std::mem::take(&mut inner.pending_actions) {
                    if action.is_lifecycle() {
                        lifecycle.push(action);
                    } else {
                        remaining.push(action);
                    }
                }
                inner.pending_actions = remaining;
                lifecycle
            })
            .unwrap_or_default()
    }

    /// Alias with an explicit name for mode integrations that need the
    /// continuation-bearing form rather than the legacy payload-only drain.
    pub fn drain_pending_lifecycle_actions_with_metadata(&self) -> Vec<PendingHostAction> {
        self.drain_pending_lifecycle_action_metadata()
    }

    /// Drain all queued actions with their original arguments and optional
    /// bridge continuation metadata intact.
    pub fn drain_pending_action_metadata(&self) -> Vec<PendingHostAction> {
        self.inner
            .lock()
            .map(|mut inner| std::mem::take(&mut inner.pending_actions))
            .unwrap_or_default()
    }

    /// Install the callback that emits a lifecycle completion after the mode
    /// has applied a drained action. The callback is invoked outside all host
    /// state locks so it may safely write to the bridge.
    pub fn set_lifecycle_completion_sink(&self, sink: LifecycleCompletionSink) {
        if let Ok(mut current) = self.lifecycle_completion_sink.lock() {
            *current = Some(sink);
        }
    }

    /// Complete a lifecycle request after the mode has applied it.
    ///
    /// The request is removed only after a configured sink accepts the result.
    /// Without a sink this returns an explicit error and leaves the request
    /// pending, so a mode cannot accidentally claim a bridge completion that
    /// has nowhere to go.
    pub fn complete_lifecycle_action(
        &self,
        request: PendingHostAction,
        result: Value,
    ) -> Result<(), String> {
        if !request.is_lifecycle() {
            return Err(format!(
                "{} is not a lifecycle host action",
                request.action.protocol_name()
            ));
        }
        let sink = self
            .lifecycle_completion_sink
            .lock()
            .map_err(|_| "Extension lifecycle completion sink lock poisoned".to_string())?
            .clone();
        let sink = sink.ok_or_else(|| {
            "No lifecycle completion sink configured; action remains pending".to_string()
        })?;
        sink(request.clone(), result)?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Extension host state lock poisoned".to_string())?;
        if let Some(index) = inner
            .pending_actions
            .iter()
            .position(|pending| pending == &request)
        {
            inner.pending_actions.remove(index);
        }
        Ok(())
    }

    pub fn drain_pending_tool_updates(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|mut inner| {
                std::mem::take(&mut inner.pending_tool_updates)
                    .into_iter()
                    .map(|(_, update)| update)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    fn snapshot_value_for(&self, tool_call_id: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("extension host state lock");
        let editor_text = self.ui_broker.editor_text();
        let ui_state = self.ui_broker.ui_state_snapshot();
        let signal = tool_call_id
            .and_then(|tool_call_id| inner.tool_signals.get(tool_call_id))
            .or(inner.signal.as_ref())
            .map(|signal| {
                json!({
                    "aborted": signal.load(Ordering::Acquire),
                })
            });
        json!({
            "sessionName": inner.session_name,
            "activeTools": inner.active_tools,
            "allTools": inner.all_tools,
            "commands": inner.commands,
            "thinkingLevel": inner.thinking_level,
            "model": inner.model,
            "scopedModels": inner.scoped_models,
            "isIdle": inner.is_idle,
            "isProjectTrusted": inner.is_project_trusted,
            "signal": signal,
            "hasPendingMessages": inner.has_pending_messages,
            "contextUsage": inner.context_usage,
            "systemPrompt": inner.system_prompt,
            "systemPromptOptions": inner.system_prompt_options,
            "editorText": editor_text,
            "uiState": ui_state,
        })
    }

    fn snapshot_value(&self) -> Value {
        self.snapshot_value_for(None)
    }

    /// Return the current host snapshot without requiring callers to import
    /// the object-safe action trait for a point-in-time read.
    pub fn snapshot(&self) -> Value {
        self.snapshot_value()
    }

    fn dispatch_action(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String> {
        match self.dispatch_with_outcome(action, args)? {
            ExtensionHostActionOutcome::Completed(result) => Ok(result),
            ExtensionHostActionOutcome::Pending(request) => Ok(match request.action {
                // Native callers use `dispatch` as the fire-and-forget host
                // surface. Preserve the upstream provisional lifecycle shape;
                // the external bridge uses `dispatch_with_outcome` and keeps
                // the request pending until the mode sends its completion.
                ExtensionHostAction::NewSession
                | ExtensionHostAction::Fork
                | ExtensionHostAction::NavigateTree
                | ExtensionHostAction::SwitchSession => json!({"cancelled": false}),
                ExtensionHostAction::Reload => Value::Null,
                ExtensionHostAction::SetModel => Value::Bool(true),
                _ => unreachable!("non-queued host action returned Pending"),
            }),
        }
    }

    /// Dispatch a host action without collapsing queued work into a success
    /// value. This is the explicit seam the mode/loader bridge uses to retain
    /// a request and complete its bridge promise later.
    pub fn dispatch_with_outcome(
        &self,
        action: ExtensionHostAction,
        args: &Value,
    ) -> Result<ExtensionHostActionOutcome, String> {
        if action == ExtensionHostAction::WaitForIdle {
            self.wait_for_idle()?;
            return Ok(ExtensionHostActionOutcome::Completed(Value::Null));
        }

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Extension host state lock poisoned".to_string())?;
        let pending = |inner: &mut ExtensionHostStateInner, payload: Value| {
            let request = PendingHostAction::new(action, args.clone(), payload);
            inner.pending_actions.push(request.clone());
            ExtensionHostActionOutcome::Pending(request)
        };
        match action {
            ExtensionHostAction::NewSession => Ok(pending(
                &mut inner,
                json!({
                    "type": "new_session",
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }),
            )),
            ExtensionHostAction::Fork => Ok(pending(
                &mut inner,
                json!({
                    "type": "fork",
                    "entryId": args.get("entryId").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }),
            )),
            ExtensionHostAction::NavigateTree => Ok(pending(
                &mut inner,
                json!({
                    "type": "navigate_tree",
                    "targetId": args.get("targetId").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }),
            )),
            ExtensionHostAction::SwitchSession => Ok(pending(
                &mut inner,
                json!({
                    "type": "switch_session",
                    "sessionPath": args.get("sessionPath").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }),
            )),
            ExtensionHostAction::Reload => Ok(pending(
                &mut inner,
                json!({
                    "type": "reload",
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }),
            )),
            ExtensionHostAction::SetModel => {
                let model = args.get("model").cloned();
                inner.requested_model = model.clone();
                let request = PendingHostAction::new(
                    action,
                    args.clone(),
                    json!({
                        "type": "set_model",
                        "model": model.unwrap_or(Value::Null),
                    }),
                );
                inner.requested_model_request = Some(request.clone());
                Ok(ExtensionHostActionOutcome::Pending(request))
            }
            _ => Ok(ExtensionHostActionOutcome::Completed(
                self.dispatch_immediate_action_locked(&mut inner, action, args)?,
            )),
        }
    }

    fn dispatch_immediate_action_locked(
        &self,
        inner: &mut ExtensionHostStateInner,
        action: ExtensionHostAction,
        args: &Value,
    ) -> Result<Value, String> {
        match action {
            ExtensionHostAction::WaitForIdle
            | ExtensionHostAction::NewSession
            | ExtensionHostAction::Fork
            | ExtensionHostAction::NavigateTree
            | ExtensionHostAction::SwitchSession
            | ExtensionHostAction::Reload
            | ExtensionHostAction::SetModel => {
                unreachable!("queued host action must be handled before immediate dispatch")
            }
            ExtensionHostAction::SendMessage => {
                inner.pending_messages.push(json!({
                    "type": "send_message",
                    "message": args.get("message").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }));
                Ok(Value::Null)
            }
            ExtensionHostAction::SendUserMessage => {
                inner.pending_messages.push(json!({
                    "type": "send_user_message",
                    "content": args.get("content").cloned().unwrap_or(Value::Null),
                    "options": args.get("options").cloned().unwrap_or(Value::Null),
                }));
                Ok(Value::Null)
            }
            ExtensionHostAction::AppendEntry => {
                inner.pending_entries.push(json!({
                    "customType": args.get("customType").cloned().unwrap_or(Value::Null),
                    "data": args.get("data").cloned().unwrap_or(Value::Null),
                }));
                Ok(Value::Null)
            }
            ExtensionHostAction::SetSessionName => {
                inner.session_name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                Ok(Value::Null)
            }
            ExtensionHostAction::GetSessionName => Ok(inner
                .session_name
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null)),
            ExtensionHostAction::SetLabel => {
                inner.labels.push(args.clone());
                Ok(Value::Null)
            }
            ExtensionHostAction::GetActiveTools => Ok(Value::Array(
                inner
                    .active_tools
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )),
            ExtensionHostAction::GetAllTools => Ok(Value::Array(inner.all_tools.clone())),
            ExtensionHostAction::SetActiveTools => {
                let active_tools = args
                    .get("toolNames")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect();
                inner.requested_active_tools = Some(active_tools);
                Ok(Value::Null)
            }
            ExtensionHostAction::GetCommands => Ok(Value::Array(inner.commands.clone())),
            ExtensionHostAction::GetThinkingLevel => {
                Ok(Value::String(inner.thinking_level.clone()))
            }
            ExtensionHostAction::SetThinkingLevel => {
                if let Some(level) = args.get("level").and_then(Value::as_str) {
                    inner.thinking_level = level.to_string();
                }
                Ok(Value::Null)
            }
            ExtensionHostAction::GetModel => Ok(inner.model.clone().unwrap_or(Value::Null)),
            ExtensionHostAction::GetScopedModels => Ok(Value::Array(inner.scoped_models.clone())),
            ExtensionHostAction::IsIdle => Ok(Value::Bool(inner.is_idle)),
            ExtensionHostAction::IsProjectTrusted => Ok(Value::Bool(inner.is_project_trusted)),
            ExtensionHostAction::GetSignal => {
                let signal = args
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|tool_call_id| inner.tool_signals.get(tool_call_id))
                    .or(inner.signal.as_ref());
                Ok(signal
                    .map(|signal| json!({"aborted": signal.load(Ordering::Acquire)}))
                    .unwrap_or(Value::Null))
            }
            ExtensionHostAction::Abort => {
                let tool_call_id = args.get("toolCallId").and_then(Value::as_str);
                if let Some(signal) = tool_call_id
                    .and_then(|tool_call_id| inner.tool_signals.get(tool_call_id))
                    .or(inner.signal.as_ref())
                {
                    signal.store(true, Ordering::Release);
                }
                let mut action = serde_json::Map::new();
                action.insert("type".to_string(), Value::String("abort".to_string()));
                if let Some(tool_call_id) = tool_call_id {
                    action.insert(
                        "toolCallId".to_string(),
                        Value::String(tool_call_id.to_string()),
                    );
                }
                inner.pending_actions.push(PendingHostAction::new(
                    ExtensionHostAction::Abort,
                    args.clone(),
                    Value::Object(action),
                ));
                Ok(Value::Null)
            }
            ExtensionHostAction::HasPendingMessages => Ok(Value::Bool(inner.has_pending_messages)),
            ExtensionHostAction::Shutdown => {
                let mut action = serde_json::Map::new();
                action.insert("type".to_string(), Value::String("shutdown".to_string()));
                if let Some(tool_call_id) = args.get("toolCallId").and_then(Value::as_str) {
                    action.insert(
                        "toolCallId".to_string(),
                        Value::String(tool_call_id.to_string()),
                    );
                }
                inner.pending_actions.push(PendingHostAction::new(
                    ExtensionHostAction::Shutdown,
                    args.clone(),
                    Value::Object(action),
                ));
                Ok(Value::Null)
            }
            ExtensionHostAction::GetContextUsage => {
                Ok(inner.context_usage.clone().unwrap_or(Value::Null))
            }
            ExtensionHostAction::Compact => {
                let mut action = serde_json::Map::new();
                action.insert("type".to_string(), Value::String("compact".to_string()));
                action.insert(
                    "options".to_string(),
                    args.get("options").cloned().unwrap_or(Value::Null),
                );
                if let Some(tool_call_id) = args.get("toolCallId").and_then(Value::as_str) {
                    action.insert(
                        "toolCallId".to_string(),
                        Value::String(tool_call_id.to_string()),
                    );
                }
                inner.pending_actions.push(PendingHostAction::new(
                    ExtensionHostAction::Compact,
                    args.clone(),
                    Value::Object(action),
                ));
                Ok(Value::Null)
            }
            ExtensionHostAction::GetSystemPrompt => Ok(Value::String(inner.system_prompt.clone())),
            ExtensionHostAction::GetSystemPromptOptions => Ok(inner.system_prompt_options.clone()),
            ExtensionHostAction::ToolUpdate => {
                let tool_call_id = args
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                inner.pending_tool_updates.push((
                    tool_call_id,
                    args.get("result").cloned().unwrap_or(Value::Null),
                ));
                Ok(Value::Null)
            }
        }
    }
}

impl ExtensionHostActions for ExtensionHostState {
    fn dispatch(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String> {
        self.dispatch_action(action, args)
    }

    fn ui_broker(&self) -> ExtensionUiBroker {
        ExtensionHostState::ui_broker(self)
    }

    fn dispatch_with_outcome(
        &self,
        action: ExtensionHostAction,
        args: &Value,
    ) -> Result<ExtensionHostActionOutcome, String> {
        ExtensionHostState::dispatch_with_outcome(self, action, args)
    }

    fn set_lifecycle_completion_sink(&self, sink: LifecycleCompletionSink) {
        ExtensionHostState::set_lifecycle_completion_sink(self, sink);
    }

    fn snapshot(&self) -> Value {
        self.snapshot_value()
    }

    fn snapshot_for(&self, request: &Value) -> Value {
        self.snapshot_value_for(request.get("toolCallId").and_then(Value::as_str))
    }
}

/// A loaded, mode-scoped extension runtime.
#[derive(Clone)]
pub struct LoadedExtensions {
    pub runner: Arc<ExtensionRunner>,
    pub host: Arc<ExtensionHostState>,
    pub runtime: Arc<Mutex<super::types::ExtensionRuntime>>,
    pub errors: Vec<ExtensionLoadError>,
    /// Temporary resource paths returned by the startup/reload
    /// `resources_discover` event.  The mode owns how these paths are folded
    /// into its prompt/theme loader.
    pub resources: ResourceDiscovery,
}

/// Load extensions using the same project/global/explicit path policy as the
/// upstream resource loader.  `--no-extensions` retains only explicit `-e`
/// paths, matching the CLI contract.
#[allow(clippy::too_many_arguments)] // explicit mode/session binding mirrors the upstream runtime context
pub fn load_for_mode(
    args: &Args,
    settings: &SettingsManager,
    cwd: &str,
    agent_dir: &str,
    mode: &str,
    has_ui: bool,
    session_name: Option<String>,
    thinking_level: impl Into<String>,
) -> LoadedExtensions {
    load_for_mode_with_reason(
        args,
        settings,
        cwd,
        agent_dir,
        mode,
        has_ui,
        session_name,
        thinking_level,
        "startup",
    )
}

/// Load a mode-scoped extension runtime and emit its session lifecycle
/// startup event.  Reload callers use `reason = "reload"` so extensions can
/// distinguish a fresh process from a resource refresh.
#[allow(clippy::too_many_arguments)]
pub fn load_for_mode_with_reason(
    args: &Args,
    settings: &SettingsManager,
    cwd: &str,
    agent_dir: &str,
    mode: &str,
    has_ui: bool,
    session_name: Option<String>,
    thinking_level: impl Into<String>,
    reason: &str,
) -> LoadedExtensions {
    load_for_mode_with_reason_and_flags(
        args,
        settings,
        cwd,
        agent_dir,
        mode,
        has_ui,
        session_name,
        thinking_level,
        reason,
        parsed_extension_flag_values(args),
    )
}

/// Variant used by reload/session replacement paths to seed the newly loaded
/// runtime before any lifecycle or resource-discovery handler runs.
#[allow(clippy::too_many_arguments)]
pub fn load_for_mode_with_reason_and_flags(
    args: &Args,
    settings: &SettingsManager,
    cwd: &str,
    agent_dir: &str,
    mode: &str,
    has_ui: bool,
    session_name: Option<String>,
    thinking_level: impl Into<String>,
    reason: &str,
    flag_values: Option<BTreeMap<String, Value>>,
) -> LoadedExtensions {
    load_for_mode_with_reason_and_flags_and_previous(
        args,
        settings,
        cwd,
        agent_dir,
        mode,
        has_ui,
        session_name,
        thinking_level,
        reason,
        flag_values,
        None,
    )
}

/// Full lifecycle variant for session replacement.  The previous session
/// file is included in `session_start` while preserved flags are seeded before
/// either lifecycle or resource handlers execute.
#[allow(clippy::too_many_arguments)]
pub fn load_for_mode_with_reason_and_flags_and_previous(
    args: &Args,
    settings: &SettingsManager,
    cwd: &str,
    agent_dir: &str,
    mode: &str,
    has_ui: bool,
    session_name: Option<String>,
    thinking_level: impl Into<String>,
    reason: &str,
    flag_values: Option<BTreeMap<String, Value>>,
    previous_session_file: Option<&str>,
) -> LoadedExtensions {
    let host = Arc::new(ExtensionHostState::new(session_name, thinking_level));
    // RPC installs its output sink only after the stdin/stdout loop exists.
    // Enable its broker early so startup lifecycle fire-and-forget actions are
    // retained in the outbox; `runner.create_context()` makes dialog calls
    // fail fast until a worker-safe callback is running. The RPC mode binds
    // the sink immediately before entering its loop and flushes the retained
    // actions then. Other modes need their own terminal UI sink and therefore
    // remain explicit until that adapter is installed.
    host.set_ui_enabled(has_ui && mode == "rpc");
    host.set_system_prompt_options(json!({"cwd": cwd}));
    let mut configured_paths = args.extensions.clone();
    if !args.no_extensions {
        configured_paths.extend(settings.get_extension_paths());
    }
    let result = if args.no_extensions {
        load_extensions_with_host_actions(&configured_paths, cwd, None, None, host.clone())
    } else {
        let agent_dir = agent_dir.to_string();
        let result = discover_and_load_extensions(&configured_paths, cwd, &agent_dir, None, None);
        result.bind_core_with_actions(host.clone());
        result
    };
    let runtime = result.runtime.clone();
    if let Some(flag_values) = flag_values {
        if let Ok(mut runtime) = runtime.lock() {
            runtime.flag_values.extend(flag_values);
        }
    }
    let mut runner = ExtensionRunner::new(result.extensions, result.runtime, cwd.to_string());
    runner.set_ui_context(mode, has_ui);
    let runner = Arc::new(runner);
    if let Err(errors) = runner.emit_session_start_with_previous(reason, previous_session_file) {
        for error in errors {
            tracing::warn!(
                extension = %error.extension_path,
                event = %error.event,
                error = %error.error,
                "extension lifecycle handler failed"
            );
        }
    }
    let resources = runner.emit_resources_discover(&json!({
        "type": "resources_discover",
        "cwd": cwd,
        "reason": if reason == "reload" { "reload" } else { "startup" },
    }));
    LoadedExtensions {
        runner,
        host,
        runtime,
        errors: result.errors,
        resources,
    }
}

/// Add live extension tools to a mode's base tool vector and publish the
/// resulting catalog to synchronous extension getters.
pub fn install_tools(
    loaded: &LoadedExtensions,
    tools: &mut Vec<AgentTool>,
    include_extensions: bool,
) {
    let mut all_tools = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.tool.name,
                "description": tool.tool.description,
                "parameters": tool.tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    let mut active_tools = tools
        .iter()
        .map(|tool| tool.tool.name.clone())
        .collect::<Vec<_>>();

    let extension_definitions = loaded.runner.get_all_registered_tools();
    let extension_tools = extension_definitions
        .iter()
        .cloned()
        .map(|registered| {
            extension_agent_tool(registered, loaded.runner.clone(), loaded.host.clone())
        })
        .collect::<Vec<_>>();
    for definition in &extension_definitions {
        all_tools.push(registered_tool_catalog_value(definition));
        if include_extensions {
            active_tools.push(definition.name.clone());
        }
    }
    let mut command_runner = loaded.runner.as_ref().clone();
    let commands = command_runner
        .get_registered_commands()
        .into_iter()
        .map(|command| {
            json!({
                "name": command.invocation_name,
                "description": command.description,
            })
        })
        .collect();
    loaded.host.set_catalog(active_tools, all_tools, commands);
    if include_extensions {
        tools.extend(extension_tools);
    }
}

fn registered_tool_catalog_value(registered: &RegisteredTool) -> Value {
    let mut catalog = json!({
        "name": registered.name,
        "label": registered.label,
        "description": registered.description,
        "parameters": registered.parameters,
        "renderShell": registered.render_shell.protocol_name(),
        "sourceInfo": {
            "path": registered.source_info.path,
            "source": registered.source_info.source,
            "scope": registered.source_info.scope,
            "origin": registered.source_info.origin,
            "baseDir": registered.source_info.base_dir,
        },
    });
    let Some(catalog) = catalog.as_object_mut() else {
        return catalog;
    };
    if let Some(prompt_snippet) = &registered.prompt_snippet {
        catalog.insert(
            "promptSnippet".to_string(),
            Value::String(prompt_snippet.clone()),
        );
    }
    if let Some(prompt_guidelines) = &registered.prompt_guidelines {
        catalog.insert(
            "promptGuidelines".to_string(),
            Value::Array(
                prompt_guidelines
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    if let Some(constrained_sampling) = &registered.constrained_sampling {
        catalog.insert(
            "constrainedSampling".to_string(),
            constrained_sampling.clone(),
        );
    }
    if let Some(execution_mode) = registered.execution_mode {
        catalog.insert(
            "executionMode".to_string(),
            Value::String(execution_mode.protocol_name().to_string()),
        );
    }
    Value::Object(catalog.clone())
}

fn extension_agent_tool(
    registered: RegisteredTool,
    runner: Arc<ExtensionRunner>,
    host: Arc<ExtensionHostState>,
) -> AgentTool {
    let tool_name = registered.name.clone();
    let tool = json_tool(
        &registered.name,
        &registered.description,
        &registered.parameters,
    );
    let mut tool = tool;
    tool.constrained_sampling = registered
        .constrained_sampling
        .as_ref()
        .and_then(native_constrained_sampling);
    let execute: ToolExecuteFn = Arc::new(
        move |tool_call_id,
              params,
              signal: Option<Arc<AtomicBool>>,
              on_update: Option<ToolUpdateCallback>| {
            let runner = runner.clone();
            let host = host.clone();
            let tool_name = tool_name.clone();
            Box::pin(async move {
                if signal
                    .as_ref()
                    .is_some_and(|signal| signal.load(Ordering::Acquire))
                {
                    return Err("Operation aborted".to_string());
                }
                let before = host.active_tools();
                host.begin_tool_execution(&tool_call_id, signal.clone());
                let extension_update: Option<ToolUpdateFn> = on_update.clone().map(|on_update| {
                    Arc::new(move |value: Value| {
                        let result = extension_tool_result(value)?;
                        on_update(&result);
                        Ok(())
                    }) as ToolUpdateFn
                });
                let execution_tool_call_id = tool_call_id.clone();
                let execution = tokio::task::spawn_blocking(move || {
                    runner.execute_tool_prepared_with_updates(
                        &tool_name,
                        &execution_tool_call_id,
                        params,
                        extension_update,
                    )
                })
                .await;
                let pending_updates = host.end_tool_execution(&tool_call_id);
                let value =
                    execution.map_err(|error| format!("extension tool task failed: {error}"))??;
                let mut result = extension_tool_result(value)?;
                let after = host.active_tools();
                let before_set = before.iter().map(String::as_str).collect::<BTreeSet<_>>();
                if before
                    .iter()
                    .all(|name| after.iter().any(|value| value == name))
                {
                    let mut seen = BTreeSet::new();
                    result
                        .added_tool_names
                        .retain(|name| seen.insert(name.clone()));
                    for name in after
                        .iter()
                        .filter(|name| !before_set.contains(name.as_str()))
                    {
                        if seen.insert(name.clone()) {
                            result.added_tool_names.push(name.clone());
                        }
                    }
                }
                if let Some(on_update) = on_update {
                    for update in pending_updates {
                        let update = extension_tool_result(update)?;
                        on_update(&update);
                    }
                    on_update(&result);
                }
                Ok(result)
            })
        },
    );
    let mut agent_tool = AgentTool::new(tool, registered.label.clone(), execute);
    if let Some(prepare_arguments) = registered.prepare_arguments {
        let prepare_arguments: AgentToolPrepareArgumentsFn =
            Arc::new(move |params| prepare_arguments(params));
        agent_tool = agent_tool.with_prepare_arguments(prepare_arguments);
    }
    if let Some(execution_mode) = registered.execution_mode {
        let execution_mode = match execution_mode {
            ToolExecutionMode::Sequential => AgentToolExecutionMode::Sequential,
            ToolExecutionMode::Parallel => AgentToolExecutionMode::Parallel,
        };
        agent_tool = agent_tool.with_execution_mode(execution_mode);
    }
    agent_tool
}

fn native_constrained_sampling(value: &Value) -> Option<ConstrainedSampling> {
    let object = value.as_object()?;
    match object.get("type").and_then(Value::as_str)? {
        "json_schema" => {
            let strict = match object.get("strict").and_then(Value::as_str) {
                Some("prefer") => StrictPreference::Prefer,
                Some("require") => StrictPreference::Require,
                _ => return None,
            };
            Some(ConstrainedSampling::JsonSchema { strict })
        }
        "grammar" => {
            let variants = object
                .get("variants")
                .and_then(Value::as_object)?
                .iter()
                .map(|(name, definition)| Some((name.clone(), definition.as_str()?.to_string())))
                .collect::<Option<BTreeMap<_, _>>>()?;
            Some(ConstrainedSampling::Grammar { variants })
        }
        _ => None,
    }
}

fn extension_tool_result(value: Value) -> Result<AgentToolResult, String> {
    let value = normalize_tool_result(value);
    let content = match value.get("content") {
        None | Some(Value::Null) => Vec::new(),
        Some(content) => match serde_json::from_value::<Vec<ContentBlock>>(content.clone()) {
            Ok(content) => content,
            Err(_) => content
                .as_str()
                .map(|text| vec![ContentBlock::text(text)])
                .unwrap_or_else(|| vec![ContentBlock::text(compact_json(&value))]),
        },
    };
    let details = value.get("details").cloned();
    let usage = value
        .get("usage")
        .cloned()
        .and_then(|usage| serde_json::from_value(usage).ok());
    let added_tool_names = value
        .get("addedToolNames")
        .or_else(|| value.get("added_tool_names"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    let terminate = value
        .get("terminate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = AgentToolResult {
        content,
        details,
        usage,
        added_tool_names,
        terminate,
    };
    if value
        .get("isError")
        .or_else(|| value.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(extension_tool_error(&value, &result));
    }
    Ok(result)
}

fn normalize_tool_result(value: Value) -> Value {
    let Value::Object(mut outer) = value else {
        return value;
    };
    let Some(inner) = outer.remove("result") else {
        return Value::Object(outer);
    };
    let Value::Object(mut inner) = inner else {
        outer.insert("result".to_string(), inner);
        return Value::Object(outer);
    };
    for (key, value) in outer {
        inner.entry(key).or_insert(value);
    }
    Value::Object(inner)
}

fn extension_tool_error(value: &Value, result: &AgentToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        compact_json(value)
    } else {
        text
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "Extension tool completed".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::core::extensions::types::ToolExecutionRequest;
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pi-extension-integration-{name}-{}",
            std::process::id()
        ))
    }

    fn loaded_native_extension<F>(
        cwd: &str,
        mode: &str,
        has_ui: bool,
        factory: F,
    ) -> LoadedExtensions
    where
        F: for<'a> FnOnce(
                &mut crate::core::extensions::loader::ExtensionApi<'a>,
            ) -> Result<(), String>
            + 'static,
    {
        let runtime = crate::core::extensions::loader::create_extension_runtime();
        let extension = crate::core::extensions::loader::load_extension_from_factory(
            factory,
            cwd,
            runtime.clone(),
            "<inline:integration>",
        )
        .expect("Rust-native extension factory");
        let host = Arc::new(ExtensionHostState::new(None, "medium"));
        let mut runner = ExtensionRunner::new(vec![extension], runtime.clone(), cwd.to_string());
        runner.set_ui_context(mode, has_ui);
        runner.bind_core_with_actions(host.clone());
        LoadedExtensions {
            runner: Arc::new(runner),
            host,
            runtime,
            errors: Vec::new(),
            resources: ResourceDiscovery::default(),
        }
    }

    #[test]
    fn default_mode_loader_seeds_parsed_extension_flags_before_lifecycle() {
        let args = Args {
            extension_flag_values: vec![
                (
                    "native-switch".to_string(),
                    ExtensionFlagValue::Boolean(true),
                ),
                (
                    "native-label".to_string(),
                    ExtensionFlagValue::String("first".to_string()),
                ),
                (
                    "native-label".to_string(),
                    ExtensionFlagValue::String("last".to_string()),
                ),
            ],
            ..Default::default()
        };
        let settings = SettingsManager::in_memory(Default::default());
        let loaded = load_for_mode(
            &args,
            &settings,
            "/tmp/pi-extension-flags",
            "/tmp/pi-extension-flags-agent",
            "print",
            false,
            None,
            "medium",
        );

        assert_eq!(
            parsed_extension_flag_values(&args),
            Some(BTreeMap::from([
                ("native-label".to_string(), json!("last")),
                ("native-switch".to_string(), json!(true)),
            ]))
        );
        assert_eq!(
            loaded.runner.get_flag_values(),
            BTreeMap::from([
                ("native-label".to_string(), json!("last")),
                ("native-switch".to_string(), json!(true)),
            ])
        );
        assert!(loaded
            .host
            .drain_pending_lifecycle_action_metadata()
            .is_empty());
        loaded
            .runner
            .invalidate(Some("flag propagation test complete"));
    }

    #[test]
    fn no_extensions_keeps_cli_paths_and_suppresses_settings_paths() {
        let root = fixture_root("no-extensions-precedence");
        let cli_path = root.join("cli.ts").to_string_lossy().into_owned();
        let settings_path = root.join("settings.ts").to_string_lossy().into_owned();
        let mut settings_values = crate::core::settings::SettingsMap::new();
        settings_values.insert("extensions".to_string(), json!([settings_path.clone()]));
        let settings = SettingsManager::in_memory(settings_values);

        let cli_only_args = Args {
            extensions: vec![cli_path.clone()],
            no_extensions: true,
            ..Default::default()
        };
        let cli_only = load_for_mode(
            &cli_only_args,
            &settings,
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "print",
            false,
            None,
            "medium",
        );
        assert_eq!(
            cli_only
                .errors
                .iter()
                .map(|error| error.path.clone())
                .collect::<Vec<_>>(),
            vec![cli_path.clone()]
        );
        cli_only
            .runner
            .invalidate(Some("no-extensions precedence test complete"));

        let all_args = Args {
            extensions: vec![cli_path.clone()],
            ..Default::default()
        };
        let all = load_for_mode(
            &all_args,
            &settings,
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "print",
            false,
            None,
            "medium",
        );
        assert_eq!(
            all.errors
                .iter()
                .map(|error| error.path.clone())
                .collect::<Vec<_>>(),
            vec![cli_path, settings_path]
        );
        all.runner
            .invalidate(Some("extension precedence test complete"));
    }

    #[test]
    fn host_lifecycle_actions_retain_pending_metadata_without_completion() {
        let host = ExtensionHostState::new(None, "medium");
        let requests = [
            (
                ExtensionHostAction::NewSession,
                json!({
                    "options": {
                        "parentSession": "parent",
                        "__bridgeContinuation": {"id": "bridge-1"},
                    },
                }),
            ),
            (
                ExtensionHostAction::Fork,
                json!({"entryId": "entry", "options": {"position": "at"}}),
            ),
            (
                ExtensionHostAction::NavigateTree,
                json!({"targetId": "leaf", "options": {"summarize": false}}),
            ),
            (
                ExtensionHostAction::SwitchSession,
                json!({"sessionPath": "/tmp/session.jsonl", "options": null}),
            ),
            (ExtensionHostAction::Reload, json!({})),
        ];
        for (action, args) in requests {
            let outcome = host
                .dispatch_with_outcome(action, &args)
                .expect("lifecycle outcome");
            match outcome {
                ExtensionHostActionOutcome::Pending(request) => {
                    assert_eq!(request.args, args);
                    assert_eq!(
                        request.payload["options"],
                        args.get("options").cloned().unwrap_or(Value::Null)
                    );
                }
                ExtensionHostActionOutcome::Completed(result) => {
                    panic!("{action:?} completed early with {result}")
                }
            }
        }
        assert_eq!(
            host.dispatch(ExtensionHostAction::Reload, &json!({})),
            Ok(Value::Null)
        );
        let metadata = host.drain_pending_lifecycle_action_metadata();
        assert_eq!(metadata.len(), 6);
        assert_eq!(metadata[0].action, ExtensionHostAction::NewSession);
        assert_eq!(
            metadata[0].continuation_metadata(),
            Some(&json!({"id": "bridge-1"}))
        );
        assert_eq!(
            metadata[0].args["options"],
            json!({
                "parentSession": "parent",
                "__bridgeContinuation": {"id": "bridge-1"},
            })
        );
        assert_eq!(
            metadata[0].payload,
            json!({
                "type": "new_session",
                "options": {
                    "parentSession": "parent",
                    "__bridgeContinuation": {"id": "bridge-1"},
                },
            })
        );
        assert_eq!(
            metadata
                .iter()
                .map(|request| request.action)
                .collect::<Vec<_>>(),
            vec![
                ExtensionHostAction::NewSession,
                ExtensionHostAction::Fork,
                ExtensionHostAction::NavigateTree,
                ExtensionHostAction::SwitchSession,
                ExtensionHostAction::Reload,
                ExtensionHostAction::Reload,
            ]
        );
    }

    #[test]
    fn host_wait_for_idle_wakes_and_times_out_with_a_bound() {
        let host = ExtensionHostState::new(None, "medium");
        host.set_idle(false);
        let waiter = host.clone();
        let join = std::thread::spawn(move || {
            waiter.wait_for_idle_timeout(std::time::Duration::from_secs(1))
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        host.set_idle(true);
        assert_eq!(join.join().expect("idle waiter thread"), Ok(()));
        assert_eq!(
            host.dispatch(ExtensionHostAction::WaitForIdle, &json!({})),
            Ok(Value::Null)
        );

        host.set_idle(false);
        let started = std::time::Instant::now();
        let error = host
            .wait_for_idle_timeout(std::time::Duration::from_millis(20))
            .expect_err("busy host must hit the wait bound");
        assert!(error.contains("Timed out waiting"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        host.set_idle(true);
    }

    #[test]
    fn host_requested_changes_are_readable_and_drained_without_mutating_snapshot() {
        let host = ExtensionHostState::new(None, "medium");
        host.set_catalog(vec!["before".to_string()], Vec::new(), Vec::new());
        host.set_model(Some(json!({"provider": "old", "id": "old-model"})));
        let model_outcome = host
            .dispatch_with_outcome(
                ExtensionHostAction::SetModel,
                &json!({
                    "model": {"provider": "new", "id": "new-model"},
                    "__bridgeContinuation": {"id": "model-1"},
                }),
            )
            .expect("model outcome");
        assert!(matches!(
            model_outcome,
            ExtensionHostActionOutcome::Pending(_)
        ));
        host.dispatch(
            ExtensionHostAction::SetActiveTools,
            &json!({"toolNames": ["after", "extension-tool"]}),
        )
        .expect("active-tool request is fire-and-forget");

        assert_eq!(
            host.snapshot()["model"],
            json!({"provider": "old", "id": "old-model"})
        );
        assert_eq!(host.active_tools(), vec!["before"]);
        assert_eq!(
            host.requested_model(),
            Some(json!({"provider": "new", "id": "new-model"}))
        );
        assert_eq!(
            host.requested_active_tools(),
            Some(vec!["after".to_string(), "extension-tool".to_string()])
        );
        assert_eq!(
            host.requested_model_change()
                .expect("model metadata")
                .continuation_metadata(),
            Some(&json!({"id": "model-1"}))
        );

        let changes = host.drain_requested_changes();
        assert_eq!(
            changes.model,
            Some(json!({"provider": "new", "id": "new-model"}))
        );
        assert_eq!(
            changes.active_tools,
            Some(vec!["after".to_string(), "extension-tool".to_string()])
        );
        assert_eq!(
            changes
                .model_request
                .expect("drained model metadata")
                .action,
            ExtensionHostAction::SetModel
        );
        assert!(host.requested_model().is_none());
        assert!(host.requested_active_tools().is_none());
    }

    #[test]
    fn lifecycle_completion_sink_receives_only_explicit_mode_completion() {
        let host = ExtensionHostState::new(None, "medium");
        let completions = Arc::new(Mutex::new(Vec::<(PendingHostAction, Value)>::new()));
        let completions_for_sink = Arc::clone(&completions);
        host.set_lifecycle_completion_sink(Arc::new(move |request, result| {
            completions_for_sink
                .lock()
                .map_err(|_| "completion lock poisoned".to_string())?
                .push((request, result));
            Ok(())
        }));
        let request = match host
            .dispatch_with_outcome(
                ExtensionHostAction::SwitchSession,
                &json!({
                    "sessionPath": "/tmp/session.jsonl",
                    "options": {
                        "withSession": null,
                        "__bridgeContinuation": {"id": "switch-2"},
                    },
                }),
            )
            .expect("switch outcome")
        {
            ExtensionHostActionOutcome::Pending(request) => request,
            ExtensionHostActionOutcome::Completed(_) => panic!("switch completed early"),
        };
        assert_eq!(
            request.continuation_metadata(),
            Some(&json!({"id": "switch-2"}))
        );
        host.complete_lifecycle_action(request.clone(), json!({"cancelled": false}))
            .expect("completion sink");
        let completions = completions.lock().expect("completion lock");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].0, request);
        assert_eq!(completions[0].1, json!({"cancelled": false}));
        assert!(host.drain_pending_lifecycle_action_metadata().is_empty());
    }

    #[test]
    fn lifecycle_completion_without_sink_stays_pending() {
        let host = ExtensionHostState::new(None, "medium");
        let request = match host
            .dispatch_with_outcome(
                ExtensionHostAction::NewSession,
                &json!({"options": {"__bridgeContinuation": {"id": "new-1"}}}),
            )
            .expect("new-session outcome")
        {
            ExtensionHostActionOutcome::Pending(request) => request,
            ExtensionHostActionOutcome::Completed(_) => panic!("new session completed early"),
        };
        let error = host
            .complete_lifecycle_action(request.clone(), json!({"cancelled": false}))
            .expect_err("completion without sink must be explicit");
        assert!(error.contains("No lifecycle completion sink configured"));
        assert_eq!(
            host.drain_pending_lifecycle_action_metadata(),
            vec![request]
        );
    }

    #[test]
    fn host_actions_trait_is_object_safe_for_pending_lifecycle_dispatch() {
        let host: Arc<dyn ExtensionHostActions> = Arc::new(ExtensionHostState::new(None, "medium"));
        host.set_lifecycle_completion_sink(Arc::new(|_, _| Ok(())));
        let outcome = host
            .dispatch_with_outcome(
                ExtensionHostAction::Reload,
                &json!({"options": {"__bridgeContinuation": {"id": "reload-1"}}}),
            )
            .expect("reload outcome");
        assert!(matches!(outcome, ExtensionHostActionOutcome::Pending(_)));
    }

    #[test]
    fn host_queues_non_reentrant_session_actions() {
        let host = ExtensionHostState::new(None, "medium");
        host.dispatch(ExtensionHostAction::Abort, &json!({}))
            .expect("abort queue");
        assert_eq!(host.drain_pending_actions(), vec![json!({"type": "abort"})]);
    }

    #[tokio::test]
    async fn native_factory_exposes_live_extension_tools_and_host_snapshot() {
        let root = fixture_root("native-tool");
        let loaded = loaded_native_extension(&root.to_string_lossy(), "print", false, |api| {
            api.register_tool(RegisteredTool {
                name: "mode-tool".to_string(),
                label: "Mode tool".to_string(),
                description: "mode integration fixture".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
                source_info: super::super::types::SourceInfo::synthetic(
                    "<inline:mode-tool>",
                    "rust-native",
                    None,
                ),
                execute: Some(Arc::new(|request| {
                    Ok(json!({
                        "content": [{"type": "text", "text": format!("{}:{}", request.tool_call_id, request.params["value"])}],
                        "details": {
                            "mode": request.context.mode,
                            "cwd": request.context.cwd,
                            "hasUi": request.context.has_ui,
                        },
                    }))
                })),
                ..Default::default()
            })?;
            api.register_command(
                "mode-command",
                Some("command".to_string()),
                Arc::new(|_, event| Ok(Some(json!({"args": event["args"]})))),
            )?;
            Ok(())
        });
        let mut tools = Vec::new();
        install_tools(&loaded, &mut tools, true);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool.name, "mode-tool");
        assert_eq!(loaded.host.snapshot()["allTools"][0]["name"], "mode-tool");
        assert_eq!(
            loaded.host.snapshot()["commands"][0]["name"],
            "mode-command"
        );

        let result = (tools[0].execute)("call-1".to_string(), json!({"value": 7}), None, None)
            .await
            .expect("extension tool execution");
        assert_eq!(result.content, vec![ContentBlock::text("call-1:7")]);
        assert_eq!(
            result.details,
            Some(json!({
                "mode": "print",
                "cwd": root.to_string_lossy(),
                "hasUi": false,
            }))
        );
        assert!(result.added_tool_names.is_empty());
        assert_eq!(loaded.host.snapshot()["activeTools"], json!(["mode-tool"]));

        loaded.runner.invalidate(Some("test complete"));
    }

    #[tokio::test]
    async fn native_factory_emits_lifecycle_before_discovering_resources() {
        let root = fixture_root("resources");
        let started = Arc::new(AtomicBool::new(false));
        let started_for_start = started.clone();
        let started_for_resources = started.clone();
        let started_for_shutdown = started.clone();
        let loaded = loaded_native_extension(&root.to_string_lossy(), "print", false, move |api| {
            api.on(
                "session_start",
                Arc::new(move |_, event| {
                    if event["reason"] != "startup" {
                        return Err("unexpected startup reason".to_string());
                    }
                    started_for_start.store(true, Ordering::Release);
                    Ok(None)
                }),
            )?;
            api.on(
                "resources_discover",
                Arc::new(move |_, event| {
                    if !started_for_resources.load(Ordering::Acquire)
                        || event["reason"] != "startup"
                    {
                        return Err("lifecycle order mismatch".to_string());
                    }
                    Ok(Some(json!({
                        "skillPaths": ["skills"],
                        "promptPaths": ["prompts/default.md"],
                        "themePaths": ["themes/dark.json"],
                    })))
                }),
            )?;
            api.on(
                "session_shutdown",
                Arc::new(move |_, event| {
                    if event["reason"] != "test" {
                        return Err("unexpected shutdown reason".to_string());
                    }
                    started_for_shutdown.store(false, Ordering::Release);
                    Ok(None)
                }),
            )?;
            Ok(())
        });
        loaded
            .runner
            .emit_session_start("startup")
            .expect("startup handler");
        let resources = loaded.runner.emit_resources_discover(&json!({
            "type": "resources_discover",
            "reason": "startup",
        }));
        assert_eq!(resources.skill_paths, vec!["skills"]);
        assert_eq!(resources.prompt_paths, vec!["prompts/default.md"]);
        assert_eq!(resources.theme_paths, vec!["themes/dark.json"]);
        loaded
            .runner
            .emit_session_shutdown("test")
            .expect("shutdown handler");
        loaded.runner.invalidate(Some("test complete"));
    }

    #[tokio::test]
    async fn native_tool_context_is_forwarded_through_the_runner() {
        let root = fixture_root("context-actions");
        let loaded = loaded_native_extension(&root.to_string_lossy(), "print", false, |api| {
            api.register_tool(RegisteredTool {
                name: "context-tool".to_string(),
                label: "Context tool".to_string(),
                description: "native context fixture".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
                source_info: super::super::types::SourceInfo::synthetic(
                    "<inline:context-tool>",
                    "rust-native",
                    None,
                ),
                execute: Some(Arc::new(|request| {
                    Ok(json!({
                        "content": [{"type": "text", "text": format!("{}:{}", request.tool_call_id, request.params["value"])}],
                        "details": {
                            "mode": request.context.mode,
                            "cwd": request.context.cwd,
                            "hasUi": request.context.has_ui,
                        },
                    }))
                })),
                ..Default::default()
            })?;
            Ok(())
        });

        let mut tools = Vec::new();
        install_tools(&loaded, &mut tools, true);
        assert_eq!(tools.len(), 1);
        let result =
            (tools[0].execute)("call-context".to_string(), json!({"value": 7}), None, None)
                .await
                .expect("extension tool execution");

        assert_eq!(result.content, vec![ContentBlock::text("call-context:7")]);
        assert_eq!(
            result.details,
            Some(json!({
                "mode": "print",
                "cwd": root.to_string_lossy(),
                "hasUi": false,
            }))
        );
        loaded
            .runner
            .invalidate(Some("context action test complete"));
    }

    #[test]
    fn native_registered_tool_contract_is_live() {
        let prepared_seen = Arc::new(Mutex::new(Vec::new()));
        let render_call_seen = Arc::new(Mutex::new(Vec::new()));
        let render_result_seen = Arc::new(Mutex::new(Vec::new()));
        let loaded = loaded_native_extension("/fixture/project", "rpc", true, {
            let prepared_seen = prepared_seen.clone();
            let render_call_seen = render_call_seen.clone();
            let render_result_seen = render_result_seen.clone();
            move |api| {
                let prepared_seen_for_prepare = prepared_seen.clone();
                let render_call_seen_for_callback = render_call_seen.clone();
                let render_result_seen_for_callback = render_result_seen.clone();
                api.register_tool(RegisteredTool {
                    name: "contract-tool".to_string(),
                    label: "Contract tool".to_string(),
                    description: "full native tool contract".to_string(),
                    prompt_snippet: Some("Use this tool for contract tests.".to_string()),
                    prompt_guidelines: Some(vec![
                        "Prepare arguments before execution.".to_string(),
                        "Report partial updates.".to_string(),
                    ]),
                    parameters: json!({
                        "type": "object",
                        "properties": {"value": {"type": "integer"}},
                        "required": ["value"],
                    }),
                    constrained_sampling: Some(json!({
                        "type": "json_schema",
                        "strict": "require",
                    })),
                    render_shell: super::super::types::ToolRenderShell::SelfRendered,
                    prepare_arguments: Some(Arc::new(move |mut params: Value| {
                        prepared_seen_for_prepare
                            .lock()
                            .expect("prepare arguments observations")
                            .push(params.clone());
                        params["prepared"] = json!(true);
                        params
                    })),
                    execution_mode: Some(ToolExecutionMode::Sequential),
                    source_info: super::super::types::SourceInfo::synthetic(
                        "<inline:contract-tool>",
                        "rust-native",
                        None,
                    ),
                    execute: Some(Arc::new(|request: ToolExecutionRequest| {
                        assert_eq!(request.params["prepared"], true);
                        request.update(json!({
                            "content": [{"type": "text", "text": "partial"}],
                            "details": {"partial": true},
                        }))?;
                        Ok(json!({
                            "content": [{"type": "text", "text": "final"}],
                            "details": {"prepared": request.params["prepared"]},
                        }))
                    })),
                    render_call: Some(Arc::new(move |request| {
                        render_call_seen_for_callback
                            .lock()
                            .expect("render call observations")
                            .push(request.clone());
                        Ok(json!({
                            "kind": "call",
                            "args": request.args,
                            "theme": request.theme,
                            "toolCallId": request.context.tool_call_id,
                        }))
                    })),
                    render_result: Some(Arc::new(move |request| {
                        render_result_seen_for_callback
                            .lock()
                            .expect("render result observations")
                            .push(request.clone());
                        Ok(json!({
                            "kind": "result",
                            "result": request.result,
                            "expanded": request.options.expanded,
                            "partial": request.options.is_partial,
                        }))
                    })),
                })?;
                Ok(())
            }
        });

        let updates = Arc::new(Mutex::new(Vec::new()));
        let updates_for_callback = updates.clone();
        let on_update: ToolUpdateFn = Arc::new(move |value| {
            updates_for_callback
                .lock()
                .expect("tool update observations")
                .push(value);
            Ok(())
        });
        let result = loaded
            .runner
            .execute_tool_with_updates(
                "contract-tool",
                "contract-call",
                json!({"value": 7}),
                Some(on_update),
            )
            .expect("full native tool execution");
        assert_eq!(result["details"]["prepared"], true);
        assert_eq!(
            updates.lock().expect("tool update values").as_slice(),
            &[json!({
                "content": [{"type": "text", "text": "partial"}],
                "details": {"partial": true},
            })]
        );
        assert_eq!(
            prepared_seen.lock().expect("prepared values").as_slice(),
            &[json!({"value": 7})]
        );

        let mut tools = Vec::new();
        install_tools(&loaded, &mut tools, true);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].label, "Contract tool");
        assert!(tools[0].prepare_arguments.is_some());
        assert!(matches!(
            tools[0].execution_mode,
            Some(AgentToolExecutionMode::Sequential)
        ));
        assert_eq!(
            tools[0].tool.constrained_sampling,
            Some(ConstrainedSampling::JsonSchema {
                strict: StrictPreference::Require,
            })
        );
        let prepared_by_agent_adapter =
            (tools[0]
                .prepare_arguments
                .as_ref()
                .expect("agent prepare callback"))(json!({"value": 9}));
        assert_eq!(prepared_by_agent_adapter["prepared"], true);

        let render_context = super::super::types::ToolRenderContext {
            args: json!({"value": 7}),
            tool_call_id: "contract-call".to_string(),
            last_component: None,
            state: json!({"state": "ready"}),
            cwd: "/fixture/project".to_string(),
            execution_started: true,
            args_complete: true,
            is_partial: false,
            expanded: true,
            show_images: false,
            is_error: false,
        };
        let rendered_call = loaded
            .runner
            .render_tool_call(
                "contract-tool",
                json!({"value": 7}),
                json!({"name": "dark"}),
                render_context.clone(),
            )
            .expect("render call")
            .expect("render call value");
        assert_eq!(rendered_call["kind"], "call");
        let rendered_result = loaded
            .runner
            .render_tool_result(
                "contract-tool",
                json!({"content": [{"type": "text", "text": "final"}]}),
                super::super::types::ToolRenderResultOptions {
                    expanded: true,
                    is_partial: false,
                },
                json!({"name": "dark"}),
                render_context,
            )
            .expect("render result")
            .expect("render result value");
        assert_eq!(rendered_result["kind"], "result");
        assert_eq!(rendered_result["expanded"], true);
        assert_eq!(
            render_call_seen
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
        assert_eq!(
            render_result_seen
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );

        let snapshot = loaded.host.snapshot();
        let catalog = snapshot["allTools"]
            .as_array()
            .expect("host tool catalog")
            .iter()
            .find(|tool| tool["name"] == "contract-tool")
            .expect("contract tool catalog entry");
        assert_eq!(catalog["label"], "Contract tool");
        assert_eq!(
            catalog["promptSnippet"],
            "Use this tool for contract tests."
        );
        assert_eq!(
            catalog["promptGuidelines"],
            json!([
                "Prepare arguments before execution.",
                "Report partial updates.",
            ])
        );
        assert_eq!(catalog["constrainedSampling"]["strict"], "require");
        assert_eq!(catalog["renderShell"], "self");
        assert_eq!(catalog["executionMode"], "sequential");
    }

    #[test]
    fn native_handler_can_call_the_bound_extension_host_context() {
        let loaded = loaded_native_extension("/fixture/project", "rpc", true, |api| {
            api.register_tool(RegisteredTool {
                name: "host-context-tool".to_string(),
                label: "Host context tool".to_string(),
                description: "exercise every native host-context capability".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
                source_info: super::super::types::SourceInfo::synthetic(
                    "<inline:host-context-tool>",
                    "rust-native",
                    None,
                ),
                execute: Some(Arc::new(|request| {
                    let host = &request.context.host;
                    let session_manager_name = host
                        .session_manager()?
                        .get_session_name()?
                        .unwrap_or_default();
                    let model_registry_tool_count = host.model_registry()?.get_all_tools()?.len();
                    host.wait_for_idle()?;
                    let new_session_pending = matches!(
                        host.new_session(Some(json!({"reason": "test"})))?,
                        ExtensionHostActionOutcome::Pending(_)
                    );
                    let fork_pending = matches!(
                        host.fork(Some("entry-1"), Some(json!({"position": "at"})))?,
                        ExtensionHostActionOutcome::Pending(_)
                    );
                    let navigate_pending = matches!(
                        host.navigate_tree(Some("leaf-1"), Some(json!({"summarize": false})))?,
                        ExtensionHostActionOutcome::Pending(_)
                    );
                    let switch_pending = matches!(
                        host.switch_session(
                            Some("/fixture/session.jsonl"),
                            Some(json!({"withSession": null})),
                        )?,
                        ExtensionHostActionOutcome::Pending(_)
                    );
                    let reload_pending = matches!(
                        host.reload(Some(json!({"reason": "test"})))?,
                        ExtensionHostActionOutcome::Pending(_)
                    );
                    let send_message_completed = matches!(
                        host.send_message(json!({"role": "user"}), Some(json!({"stream": false})))?,
                        ExtensionHostActionOutcome::Completed(Value::Null)
                    );
                    let send_user_message_completed = matches!(
                        host.send_user_message(
                            json!("hello"),
                            Some(json!({"deliverAs": "followUp"})),
                        )?,
                        ExtensionHostActionOutcome::Completed(Value::Null)
                    );
                    let append_entry_completed = matches!(
                        host.append_entry("custom", Some(json!({"value": 1})))?,
                        ExtensionHostActionOutcome::Completed(Value::Null)
                    );
                    host.set_session_name(Some("renamed"))?;
                    let renamed_session = host.get_session_name()?.unwrap_or_default();
                    host.set_label("entry-1", Some("important"))?;
                    let active_tools = host.get_active_tools()?;
                    let all_tools = host.get_all_tools()?;
                    host.set_active_tools(&["tool-b".to_string()])?;
                    let commands = host.get_commands()?;
                    let thinking_before = host.get_thinking_level()?;
                    host.set_thinking_level("high")?;
                    let thinking_after = host.get_thinking_level()?;
                    let model = host.model()?;
                    let model_alias = host.get_model()?;
                    let scoped_models = host.scoped_models()?;
                    let scoped_models_alias = host.get_scoped_models()?;
                    let model_change_pending = matches!(
                        host.set_model(json!({"provider": "openai", "id": "model-b"}))?,
                        ExtensionHostActionOutcome::Pending(_)
                    );
                    let is_idle = host.is_idle()?;
                    let is_project_trusted = host.is_project_trusted()?;
                    let signal_before = host.signal()?;
                    let abort_completed = matches!(
                        host.abort(None)?,
                        ExtensionHostActionOutcome::Completed(Value::Null)
                    );
                    let signal_after = host.get_signal()?;
                    let has_pending_messages = host.has_pending_messages()?;
                    let shutdown_accepted = matches!(
                        host.shutdown(None)?,
                        ExtensionHostActionOutcome::Completed(Value::Null)
                    );
                    let context_usage = host.get_context_usage()?;
                    let compact_accepted = matches!(
                        host.compact(Some(json!({"reserveTokens": 10})))?,
                        ExtensionHostActionOutcome::Completed(Value::Null)
                    );
                    let system_prompt = host.system_prompt()?;
                    let system_prompt_options = host.system_prompt_options()?;
                    host.tool_update(None, json!({"text": "progress"}))?;
                    Ok(json!({
                        "sessionManagerName": session_manager_name,
                        "modelRegistryToolCount": model_registry_tool_count,
                        "newSessionPending": new_session_pending,
                        "forkPending": fork_pending,
                        "navigatePending": navigate_pending,
                        "switchPending": switch_pending,
                        "reloadPending": reload_pending,
                        "sendMessageCompleted": send_message_completed,
                        "sendUserMessageCompleted": send_user_message_completed,
                        "appendEntryCompleted": append_entry_completed,
                        "renamedSession": renamed_session,
                        "activeTools": active_tools,
                        "allTools": all_tools,
                        "commands": commands,
                        "thinkingBefore": thinking_before,
                        "thinkingAfter": thinking_after,
                        "model": model,
                        "modelAlias": model_alias,
                        "modelChangePending": model_change_pending,
                        "scopedModels": scoped_models,
                        "scopedModelsAlias": scoped_models_alias,
                        "isIdle": is_idle,
                        "isProjectTrusted": is_project_trusted,
                        "signalBefore": signal_before,
                        "abortCompleted": abort_completed,
                        "signalAfter": signal_after,
                        "hasPendingMessages": has_pending_messages,
                        "shutdownAccepted": shutdown_accepted,
                        "contextUsage": context_usage,
                        "compactAccepted": compact_accepted,
                        "systemPrompt": system_prompt,
                        "systemPromptOptions": system_prompt_options,
                    }))
                })),
                ..Default::default()
            })?;
            Ok(())
        });
        loaded.host.set_catalog(
            vec!["tool-a".to_string()],
            vec![json!({"name": "tool-a"})],
            vec![json!({"name": "host-command"})],
        );
        loaded
            .host
            .set_model(Some(json!({"provider": "openai", "id": "model-a"})));
        loaded
            .host
            .set_scoped_models(vec![json!({"id": "model-a"}), json!({"id": "model-b"})]);
        loaded.host.set_idle(true);
        loaded.host.set_project_trusted(false);
        loaded.host.set_has_pending_messages(true);
        loaded.host.set_context_usage(Some(json!({"tokens": 42})));
        loaded.host.set_system_prompt("system");
        loaded
            .host
            .set_system_prompt_options(json!({"cwd": "/fixture/project"}));
        loaded.host.set_signal(None);
        let signal = Arc::new(AtomicBool::new(false));
        loaded
            .host
            .begin_tool_execution("host-call", Some(signal.clone()));

        let result = loaded
            .runner
            .execute_tool("host-context-tool", "host-call", json!({}))
            .expect("native host-context tool");
        let updates = loaded.host.end_tool_execution("host-call");

        assert_eq!(result["sessionManagerName"], "");
        assert_eq!(result["modelRegistryToolCount"], 1);
        assert_eq!(result["newSessionPending"], true);
        assert_eq!(result["forkPending"], true);
        assert_eq!(result["navigatePending"], true);
        assert_eq!(result["switchPending"], true);
        assert_eq!(result["reloadPending"], true);
        assert_eq!(result["sendMessageCompleted"], true);
        assert_eq!(result["sendUserMessageCompleted"], true);
        assert_eq!(result["appendEntryCompleted"], true);
        assert_eq!(result["renamedSession"], "renamed");
        assert_eq!(result["activeTools"], json!(["tool-a"]));
        assert_eq!(result["allTools"], json!([{"name": "tool-a"}]));
        assert_eq!(result["commands"], json!([{"name": "host-command"}]));
        assert_eq!(result["thinkingBefore"], "medium");
        assert_eq!(result["thinkingAfter"], "high");
        assert_eq!(
            result["model"],
            json!({"provider": "openai", "id": "model-a"})
        );
        assert_eq!(result["modelAlias"], result["model"]);
        assert_eq!(result["modelChangePending"], true);
        assert_eq!(
            result["scopedModels"],
            json!([{"id": "model-a"}, {"id": "model-b"}])
        );
        assert_eq!(result["scopedModelsAlias"], result["scopedModels"]);
        assert_eq!(result["isIdle"], true);
        assert_eq!(result["isProjectTrusted"], false);
        assert_eq!(result["signalBefore"], json!({"aborted": false}));
        assert_eq!(result["abortCompleted"], true);
        assert_eq!(result["signalAfter"], json!({"aborted": true}));
        assert!(signal.load(Ordering::Acquire));
        assert_eq!(result["hasPendingMessages"], true);
        assert_eq!(result["shutdownAccepted"], true);
        assert_eq!(result["contextUsage"], json!({"tokens": 42}));
        assert_eq!(result["compactAccepted"], true);
        assert_eq!(result["systemPrompt"], "system");
        assert_eq!(
            result["systemPromptOptions"],
            json!({"cwd": "/fixture/project"})
        );
        assert_eq!(updates, vec![json!({"text": "progress"})]);
        assert_eq!(
            loaded.host.requested_active_tools(),
            Some(vec!["tool-b".to_string()])
        );
        assert_eq!(
            loaded.host.requested_model(),
            Some(json!({"provider": "openai", "id": "model-b"}))
        );
        assert_eq!(loaded.host.drain_pending_messages().len(), 2);
        assert_eq!(
            loaded.host.drain_pending_entries(),
            vec![json!({
                "customType": "custom",
                "data": {"value": 1},
            })]
        );
        assert_eq!(loaded.host.drain_pending_lifecycle_actions().len(), 5);
        assert_eq!(loaded.host.drain_pending_actions().len(), 3);
        loaded.runner.invalidate(Some("host context test complete"));
    }

    #[test]
    fn extension_tool_result_maps_upstream_fields_and_nested_bridge_result() {
        let result = extension_tool_result(json!({
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "details": {"source": "fixture"},
                "addedToolNames": ["one", "two"],
                "terminate": true,
            },
            "details": {"outer": true},
        }))
        .expect("valid extension result");

        assert_eq!(result.content, vec![ContentBlock::text("ok")]);
        assert_eq!(result.details, Some(json!({"source": "fixture"})));
        assert_eq!(result.added_tool_names, vec!["one", "two"]);
        assert!(result.terminate);
    }

    #[test]
    fn extension_tool_result_accepts_text_content_and_reports_error_results() {
        let result = extension_tool_result(json!({"content": "plain text"}))
            .expect("string content should be adapted to a text block");
        assert_eq!(result.content, vec![ContentBlock::text("plain text")]);

        let error = extension_tool_result(json!({
            "content": [{"type": "text", "text": "tool failed"}],
            "isError": true,
        }))
        .expect_err("explicit bridge error results must fail the Rust tool call");
        assert_eq!(error, "tool failed");
    }

    #[test]
    fn host_ui_broker_forwards_and_resolves_a_real_native_request() {
        let host = ExtensionHostState::new(None, "medium");
        let broker = host.ui_broker();
        let (sender, receiver) = std::sync::mpsc::channel();
        host.set_ui_request_sink(Arc::new(move |request| {
            sender
                .send(request)
                .map_err(|_| "UI request receiver closed".to_string())
        }));
        host.set_ui_enabled(true);
        let context = super::super::types::ExtensionUiContext::new(broker.clone(), true, true);
        let worker = std::thread::spawn(move || context.input("Question", Some("hint"), None));
        let request = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("host should forward the UI request");
        assert_eq!(request["method"], "input");
        assert_eq!(request["placeholder"], "hint");
        assert_eq!(
            ExtensionHostActions::ui_broker(&host).handle_response(&json!({
                "type": "extension_ui_response",
                "id": request["id"],
                "result": "answer"
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            worker.join().expect("UI worker"),
            Ok(Some("answer".to_string()))
        );
        // Input answers are returned to the callback; only the explicit
        // editor/set-editor-text actions mutate the editor cache.
        assert_eq!(host.snapshot()["editorText"], "");
        host.ui_broker()
            .set_editor_text("draft")
            .expect("set editor text");
        assert_eq!(host.snapshot()["editorText"], "draft");
        assert!(host.ui_broker().pending_ids().is_empty());
    }
}
