//! Extension system types — port of
//! `packages/coding-agent/src/core/extensions/types.ts`.
//!
//! Rust-native extension records, registration state, callback boundaries, and
//! the correlated UI transport used by the agent modes. Handlers and renderers
//! use JSON-shaped payloads so native factories, tools, commands, and mode
//! transports share one stable API surface.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Source metadata for an extension/command (port of `core/source-info.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    pub base_dir: Option<String>,
}

impl SourceInfo {
    pub fn synthetic(path: &str, source: &str, base_dir: Option<String>) -> Self {
        Self {
            path: path.to_string(),
            source: source.to_string(),
            scope: "temporary".to_string(),
            origin: "top-level".to_string(),
            base_dir,
        }
    }
}

/// Event handler closure: receives the extension context and the event
/// payload, returns an optional result. The payload/result are opaque JSON so
/// handler dispatch stays generic (upstream `ExtensionHandler`).
pub type HandlerFn =
    Arc<dyn Fn(&ExtensionContext, &Value) -> Result<Option<Value>, String> + Send + Sync>;
type Subscription = Arc<dyn Fn() + Send + Sync>;

/// A request sink owned by a mode.  The sink is called outside the broker
/// lock, so the mode can forward the request to a pipe/channel without
/// re-entering extension state.
pub type ExtensionUiRequestSink = Arc<dyn Fn(Value) -> Result<(), String> + Send + Sync + 'static>;

/// Dialog options for the blocking Rust-native UI facade.  `HandlerFn` is a
/// synchronous callback, so this API is deliberately explicit about the
/// cancellation signal and timeout that bound the wait.
#[derive(Clone, Default)]
pub struct ExtensionUiDialogOptions {
    pub timeout: Option<Duration>,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for ExtensionUiDialogOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionUiDialogOptions")
            .field("timeout", &self.timeout)
            .field("has_cancel", &self.cancel.is_some())
            .finish()
    }
}

/// Result returned by a native terminal-input listener. `data` replaces the
/// input passed to the next listener; `consume` stops dispatch when it is
/// `Some(true)`. `None` from a listener means that it did not handle input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalInputHandlerResult {
    pub consume: Option<bool>,
    pub data: Option<String>,
}

/// Rust callback form of the upstream `TerminalInputHandler` contract.
pub type TerminalInputHandler =
    Arc<dyn Fn(&str) -> Result<Option<TerminalInputHandlerResult>, String> + Send + Sync + 'static>;

/// Result of dispatching one terminal-input payload through the registered
/// extension listeners.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalInputDispatch {
    pub data: String,
    pub consumed: bool,
    pub listener_count: usize,
    pub error_count: usize,
}

impl TerminalInputDispatch {
    pub fn as_value(&self) -> Value {
        serde_json::json!({
            "data": self.data,
            "consumed": self.consumed,
            "listenerCount": self.listener_count,
            "errorCount": self.error_count,
        })
    }
}

/// Idempotent removal handle returned by terminal-input listener registration.
/// Dropping the handle unregisters the listener, matching the upstream
/// unsubscribe function while keeping ownership and cleanup explicit in Rust.
pub struct ExtensionUiSubscription {
    broker: ExtensionUiBroker,
    id: String,
    active: Arc<AtomicBool>,
}

impl std::fmt::Debug for ExtensionUiSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionUiSubscription")
            .field("id", &self.id)
            .field("active", &self.active.load(Ordering::Acquire))
            .finish()
    }
}

impl ExtensionUiSubscription {
    fn new(broker: ExtensionUiBroker, id: String) -> Self {
        Self {
            broker,
            id,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Remove the listener immediately. The operation is safe to call more
    /// than once and is also performed automatically by `Drop`.
    pub fn unsubscribe(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.broker.remove_terminal_input_listener(&self.id);
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ExtensionUiSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Inputs supplied to Rust-native UI factories. The values intentionally stay
/// open JSON so a mode can carry theme/keybinding/component data without
/// coupling this broker to a particular terminal renderer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExtensionUiFactoryRequest {
    pub surface: String,
    pub tui: Value,
    pub theme: Value,
    pub keybindings: Value,
    pub footer_data: Option<Value>,
    pub data: Value,
}

/// Native callback form for header/footer/widget/autocomplete/editor/custom
/// factories. The returned JSON is the renderer-neutral component
/// description sent to the host or retained as host state.
pub type ExtensionUiFactoryFn =
    Arc<dyn Fn(ExtensionUiFactoryRequest) -> Result<Value, String> + Send + Sync + 'static>;

/// A UI factory can be an already materialized open-JSON component
/// description or a Rust callback that materializes one at the broker
/// boundary. This is the Rust-native representation of upstream component
/// factories; no JavaScript function or terminal component object is stored.
#[derive(Clone)]
pub enum ExtensionUiFactory {
    Json(Value),
    Callback(ExtensionUiFactoryFn),
}

impl std::fmt::Debug for ExtensionUiFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(value) => formatter.debug_tuple("Json").field(value).finish(),
            Self::Callback(_) => formatter.debug_tuple("Callback").finish(),
        }
    }
}

impl From<Value> for ExtensionUiFactory {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

impl From<ExtensionUiFactoryFn> for ExtensionUiFactory {
    fn from(callback: ExtensionUiFactoryFn) -> Self {
        Self::Callback(callback)
    }
}

/// Result shape returned by upstream `setTheme`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionUiThemeResult {
    pub success: bool,
    pub error: Option<String>,
}

impl ExtensionUiThemeResult {
    pub fn success() -> Self {
        Self {
            success: true,
            error: None,
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
        }
    }

    pub fn as_value(&self) -> Value {
        let mut value = serde_json::json!({"success": self.success});
        if let Some(error) = &self.error {
            value["error"] = Value::String(error.clone());
        }
        value
    }
}

/// Classification returned when an RPC UI response is handed to the broker.
/// Unknown, late, and fire-and-forget responses are intentionally distinct so
/// a host can diagnose a broken client without resolving another request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionUiResponseDisposition {
    Resolved,
    UnknownId,
    LateResponse,
    FireAndForgetResponse,
    Malformed,
}

/// A bounded diagnostic produced while handling the UI sub-protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionUiDiagnostic {
    pub code: String,
    pub id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionUiDialogKind {
    Select,
    Confirm,
    Input,
    Editor,
    Custom,
}

#[derive(Debug, Clone, PartialEq)]
enum ExtensionUiDialogResult {
    Value(Value),
    Cancelled,
    TimedOut,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionUiTerminalReason {
    Resolved,
    Cancelled,
    TimedOut,
    Failed,
    FireAndForget,
}

#[derive(Debug)]
struct PendingExtensionUi {
    kind: ExtensionUiDialogKind,
    response: Option<ExtensionUiDialogResult>,
}

struct ExtensionUiBrokerState {
    enabled: bool,
    pending: BTreeMap<String, PendingExtensionUi>,
    terminal: BTreeMap<String, ExtensionUiTerminalReason>,
    terminal_order: VecDeque<String>,
    outbox: VecDeque<Value>,
    diagnostics: VecDeque<ExtensionUiDiagnostic>,
    sink: Option<ExtensionUiRequestSink>,
    editor_text: String,
    terminal_input_listeners: Vec<(String, TerminalInputHandler)>,
    hidden_thinking_label: Option<String>,
    widgets: BTreeMap<String, Value>,
    header: Option<Value>,
    footer: Option<Value>,
    autocomplete_providers: Vec<Value>,
    editor_component: Option<Value>,
    themes: Vec<Value>,
    current_theme: Option<Value>,
    tools_expanded: bool,
    extension_statuses: BTreeMap<String, String>,
}

impl std::fmt::Debug for ExtensionUiBrokerState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionUiBrokerState")
            .field("enabled", &self.enabled)
            .field("pending", &self.pending)
            .field("terminal", &self.terminal)
            .field("outbox", &self.outbox.len())
            .field("diagnostics", &self.diagnostics.len())
            .field("has_sink", &self.sink.is_some())
            .field("editor_text", &self.editor_text)
            .field(
                "terminal_input_listeners",
                &self.terminal_input_listeners.len(),
            )
            .field("hidden_thinking_label", &self.hidden_thinking_label)
            .field("widgets", &self.widgets.keys().collect::<Vec<_>>())
            .field("has_header", &self.header.is_some())
            .field("has_footer", &self.footer.is_some())
            .field("autocomplete_providers", &self.autocomplete_providers.len())
            .field("has_editor_component", &self.editor_component.is_some())
            .field("themes", &self.themes.len())
            .field("current_theme", &self.current_theme)
            .field("tools_expanded", &self.tools_expanded)
            .field("extension_statuses", &self.extension_statuses)
            .finish()
    }
}

impl ExtensionUiBrokerState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: BTreeMap::new(),
            terminal: BTreeMap::new(),
            terminal_order: VecDeque::new(),
            outbox: VecDeque::new(),
            diagnostics: VecDeque::new(),
            sink: None,
            editor_text: String::new(),
            terminal_input_listeners: Vec::new(),
            hidden_thinking_label: None,
            widgets: BTreeMap::new(),
            header: None,
            footer: None,
            autocomplete_providers: Vec::new(),
            editor_component: None,
            themes: Vec::new(),
            current_theme: None,
            tools_expanded: false,
            extension_statuses: BTreeMap::new(),
        }
    }

    fn remember_terminal(&mut self, id: String, reason: ExtensionUiTerminalReason) {
        if self.terminal.insert(id.clone(), reason).is_some() {
            self.terminal_order.retain(|existing| existing != &id);
        }
        self.terminal_order.push_back(id);
        while self.terminal_order.len() > 512 {
            if let Some(oldest) = self.terminal_order.pop_front() {
                self.terminal.remove(&oldest);
            }
        }
    }

    fn diagnostic(&mut self, code: &str, id: Option<String>, message: impl Into<String>) {
        self.diagnostics.push_back(ExtensionUiDiagnostic {
            code: code.to_string(),
            id,
            message: message.into(),
        });
        while self.diagnostics.len() > 256 {
            self.diagnostics.pop_front();
        }
    }
}

struct ExtensionUiBrokerCore {
    next_id: AtomicU64,
    state: Mutex<ExtensionUiBrokerState>,
    wake: Condvar,
    status_wakeup: Arc<tokio::sync::Notify>,
}

/// Correlated request/response state for native extension UI.
///
/// Dialog requests are real blocking requests: they are inserted into
/// `pending`, emitted through the mode sink (or retained in the outbox), and
/// resolved only by a matching response, cancellation, shutdown, or bounded
/// timeout.  Fire-and-forget methods still receive an id and are remembered so
/// an accidental response can be diagnosed as such.
#[derive(Clone)]
pub struct ExtensionUiBroker {
    core: Arc<ExtensionUiBrokerCore>,
}

impl std::fmt::Debug for ExtensionUiBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.core.state.lock().map_err(|_| std::fmt::Error)?;
        formatter
            .debug_struct("ExtensionUiBroker")
            .field("enabled", &state.enabled)
            .field("pending_ids", &state.pending.keys().collect::<Vec<_>>())
            .field("queued_requests", &state.outbox.len())
            .field("diagnostics", &state.diagnostics.len())
            .finish()
    }
}

impl Default for ExtensionUiBroker {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ExtensionUiBroker {
    /// Requests without an explicit timeout are bounded by this value.  The
    /// upstream editor method has no timeout parameter; the Rust blocking
    /// facade keeps that call safe by applying this cap.
    pub const DEFAULT_DIALOG_TIMEOUT: Duration = Duration::from_secs(300);

    pub fn new() -> Self {
        Self::with_enabled(true)
    }

    pub fn disabled() -> Self {
        Self::with_enabled(false)
    }

    fn with_enabled(enabled: bool) -> Self {
        Self {
            core: Arc::new(ExtensionUiBrokerCore {
                next_id: AtomicU64::new(1),
                state: Mutex::new(ExtensionUiBrokerState::new(enabled)),
                wake: Condvar::new(),
                status_wakeup: Arc::new(tokio::sync::Notify::new()),
            }),
        }
    }

    fn next_id(&self) -> String {
        let sequence = self.core.next_id.fetch_add(1, Ordering::Relaxed);
        format!("extension-ui-{}-{sequence}", std::process::id())
    }

    pub fn is_enabled(&self) -> bool {
        self.core
            .state
            .lock()
            .map(|state| state.enabled)
            .unwrap_or(false)
    }

    /// Enable/disable mode delivery.  Disabling cancels no request by itself;
    /// callers use `cancel_all` during shutdown so waiting callbacks receive a
    /// deterministic cancellation and wake up.
    pub fn set_enabled(&self, enabled: bool) {
        let (sink, queued) = {
            let Ok(mut state) = self.core.state.lock() else {
                return;
            };
            state.enabled = enabled;
            if enabled {
                (
                    state.sink.clone(),
                    state.outbox.drain(..).collect::<Vec<_>>(),
                )
            } else {
                (None, Vec::new())
            }
        };
        if let Some(sink) = sink {
            for request in queued {
                if let Err(error) = sink(request.clone()) {
                    self.fail_dispatched_request(&request, error);
                }
            }
        }
    }

    /// Install a mode-owned output sink.  Requests made before the sink was
    /// installed remain in order and are flushed when the broker is enabled.
    pub fn set_request_sink(&self, sink: ExtensionUiRequestSink) {
        let (enabled, queued) = {
            let Ok(mut state) = self.core.state.lock() else {
                return;
            };
            state.sink = Some(sink.clone());
            if state.enabled {
                (true, state.outbox.drain(..).collect::<Vec<_>>())
            } else {
                (false, Vec::new())
            }
        };
        if enabled {
            for request in queued {
                if let Err(error) = sink(request.clone()) {
                    self.fail_dispatched_request(&request, error);
                }
            }
        }
    }

    /// Retrieve requests when a mode is using pull delivery instead of a
    /// sink.  A request remains pending until `handle_response` resolves it.
    pub fn drain_requests(&self) -> Vec<Value> {
        self.core
            .state
            .lock()
            .map(|mut state| state.outbox.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn pending_ids(&self) -> Vec<String> {
        self.core
            .state
            .lock()
            .map(|state| state.pending.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn drain_diagnostics(&self) -> Vec<ExtensionUiDiagnostic> {
        self.core
            .state
            .lock()
            .map(|mut state| state.diagnostics.drain(..).collect())
            .unwrap_or_default()
    }

    /// Return the retained extension footer rows in stable key order. Status
    /// rows are mode-owned state rather than UI requests, so they remain
    /// observable while interactive's general UI broker is disabled.
    pub fn extension_statuses(&self) -> Vec<(String, String)> {
        self.core
            .state
            .lock()
            .map(|state| {
                state
                    .extension_statuses
                    .iter()
                    .map(|(key, text)| (key.clone(), text.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wake the interactive owner when an extension status changes. The
    /// owner consumes this notification alongside terminal input so a status
    /// update is painted without waiting for another keypress.
    pub fn status_wakeup(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.core.status_wakeup)
    }

    /// Return the renderer-neutral state owned by the UI broker. This is the
    /// stateful counterpart to the fire-and-forget UI protocol and is useful
    /// to a mode adapter when it rebuilds a host view after reload.
    pub fn ui_state_snapshot(&self) -> Value {
        self.core
            .state
            .lock()
            .map(|state| {
                serde_json::json!({
                    "editorText": state.editor_text,
                    "terminalInputListenerCount": state.terminal_input_listeners.len(),
                    "hiddenThinkingLabel": state.hidden_thinking_label,
                    "widgets": state.widgets,
                    "header": state.header,
                    "footer": state.footer,
                    "autocompleteProviders": state.autocomplete_providers,
                    "editorComponent": state.editor_component,
                    "themes": state.themes,
                    "theme": state.current_theme,
                    "toolsExpanded": state.tools_expanded,
                })
            })
            .unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Register a native terminal-input listener. Listener callbacks execute
    /// outside the broker lock when dispatch_terminal_input is called by the
    /// host, so a callback may safely unregister itself or call another UI
    /// method.
    pub fn add_terminal_input_listener(
        &self,
        handler: TerminalInputHandler,
    ) -> Result<ExtensionUiSubscription, String> {
        self.ensure_enabled()?;
        let id = self.next_id();
        let mut state = self
            .core
            .state
            .lock()
            .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
        if !state.enabled {
            return Err("Extension UI is not available in this mode or during startup".to_string());
        }
        state.terminal_input_listeners.push((id.clone(), handler));
        Ok(ExtensionUiSubscription::new(self.clone(), id))
    }

    /// Remove a terminal-input listener by subscription id. Missing ids are
    /// intentionally harmless so subscription cleanup remains idempotent.
    pub fn remove_terminal_input_listener(&self, id: &str) {
        if let Ok(mut state) = self.core.state.lock() {
            state
                .terminal_input_listeners
                .retain(|(listener_id, _)| listener_id != id);
        }
    }

    pub fn terminal_input_listener_count(&self) -> usize {
        self.core
            .state
            .lock()
            .map(|state| state.terminal_input_listeners.len())
            .unwrap_or_default()
    }

    /// Dispatch terminal data in registration order. A listener can transform
    /// the data for later listeners and can stop dispatch with consume=true.
    /// Callback failures are diagnosed and do not prevent remaining listeners
    /// from seeing the input.
    pub fn dispatch_terminal_input(&self, input: &str) -> Result<TerminalInputDispatch, String> {
        self.ensure_enabled()?;
        let (listener_count, listeners) = self
            .core
            .state
            .lock()
            .map(|state| {
                (
                    state.terminal_input_listeners.len(),
                    state
                        .terminal_input_listeners
                        .iter()
                        .map(|(_, handler)| handler.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
        let mut data = input.to_string();
        let mut consumed = false;
        let mut error_count = 0;
        for handler in listeners {
            let result = catch_unwind(AssertUnwindSafe(|| handler(&data)));
            match result {
                Ok(Ok(Some(result))) => {
                    if let Some(next_data) = result.data {
                        data = next_data;
                    }
                    if result.consume == Some(true) {
                        consumed = true;
                        break;
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    error_count += 1;
                    if let Ok(mut state) = self.core.state.lock() {
                        state.diagnostic(
                            "terminal_input_handler_error",
                            None,
                            format!("terminal input listener failed: {error}"),
                        );
                    }
                }
                Err(payload) => {
                    error_count += 1;
                    if let Ok(mut state) = self.core.state.lock() {
                        state.diagnostic(
                            "terminal_input_handler_panic",
                            None,
                            format!(
                                "terminal input listener panicked: {}",
                                panic_message(payload)
                            ),
                        );
                    }
                }
            }
        }
        Ok(TerminalInputDispatch {
            data,
            consumed,
            listener_count,
            error_count,
        })
    }

    /// Remove every terminal listener during runtime invalidation/reload.
    pub fn clear_terminal_input_listeners(&self) {
        if let Ok(mut state) = self.core.state.lock() {
            state.terminal_input_listeners.clear();
        }
    }

    /// Cancel every unresolved dialog and wake all blocking callbacks.
    pub fn cancel_all(&self) {
        if let Ok(mut state) = self.core.state.lock() {
            for pending in state.pending.values_mut() {
                if pending.response.is_none() {
                    pending.response = Some(ExtensionUiDialogResult::Cancelled);
                }
            }
            self.core.wake.notify_all();
        }
    }

    fn ensure_enabled(&self) -> Result<(), String> {
        if self.is_enabled() {
            Ok(())
        } else {
            Err("Extension UI is not available in this mode or during startup".to_string())
        }
    }

    fn bounded_timeout(timeout: Option<Duration>) -> Duration {
        timeout
            .unwrap_or(Self::DEFAULT_DIALOG_TIMEOUT)
            .min(Self::DEFAULT_DIALOG_TIMEOUT)
    }

    fn materialize_factory(
        &self,
        factory: ExtensionUiFactory,
        surface: &str,
        data: Value,
    ) -> Result<Value, String> {
        match factory {
            ExtensionUiFactory::Json(value) => Ok(value),
            ExtensionUiFactory::Callback(callback) => {
                let theme = self
                    .core
                    .state
                    .lock()
                    .map(|state| state.current_theme.clone().unwrap_or(Value::Null))
                    .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
                match catch_unwind(AssertUnwindSafe(|| {
                    callback(ExtensionUiFactoryRequest {
                        surface: surface.to_string(),
                        tui: serde_json::json!({"native": true, "surface": surface}),
                        theme,
                        keybindings: serde_json::json!({}),
                        footer_data: None,
                        data,
                    })
                })) {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(error)) => Err(format!("{surface} factory failed: {error}")),
                    Err(payload) => Err(format!(
                        "{surface} factory panicked: {}",
                        panic_message(payload)
                    )),
                }
            }
        }
    }

    fn submit_dialog(
        &self,
        mut request: serde_json::Map<String, Value>,
        kind: ExtensionUiDialogKind,
        options: ExtensionUiDialogOptions,
    ) -> Result<ExtensionUiDialogResult, String> {
        self.ensure_enabled()?;
        let id = self.next_id();
        request.insert(
            "type".to_string(),
            Value::String("extension_ui_request".to_string()),
        );
        request.insert("id".to_string(), Value::String(id.clone()));
        let timeout = Self::bounded_timeout(options.timeout);
        let sink = {
            let mut state = self
                .core
                .state
                .lock()
                .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
            if !state.enabled {
                return Err(
                    "Extension UI is not available in this mode or during startup".to_string(),
                );
            }
            state.pending.insert(
                id.clone(),
                PendingExtensionUi {
                    kind,
                    response: None,
                },
            );
            if let Some(sink) = state.sink.clone() {
                Some(sink)
            } else {
                state.outbox.push_back(Value::Object(request.clone()));
                None
            }
        };
        if let Some(sink) = sink {
            if let Err(error) = sink(Value::Object(request.clone())) {
                self.fail_dispatched_request(&Value::Object(request), error);
            }
        }
        self.wait_for_dialog(&id, timeout, options.cancel)
    }

    fn fail_dispatched_request(&self, request: &Value, error: String) {
        let Some(id) = request.get("id").and_then(Value::as_str) else {
            return;
        };
        if let Ok(mut state) = self.core.state.lock() {
            if let Some(pending) = state.pending.get_mut(id) {
                pending.response = Some(ExtensionUiDialogResult::Error(error));
                self.core.wake.notify_all();
            } else {
                state.diagnostic(
                    "request_delivery_failed",
                    Some(id.to_string()),
                    "Extension UI request sink rejected the request",
                );
            }
        }
    }

    fn wait_for_dialog(
        &self,
        id: &str,
        timeout: Duration,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<ExtensionUiDialogResult, String> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .core
            .state
            .lock()
            .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
        loop {
            let response = {
                let pending = state
                    .pending
                    .get_mut(id)
                    .ok_or_else(|| format!("Extension UI request {id} disappeared"))?;
                if let Some(response) = pending.response.take() {
                    Some(response)
                } else if cancel
                    .as_ref()
                    .is_some_and(|signal| signal.load(Ordering::Acquire))
                {
                    pending.response = Some(ExtensionUiDialogResult::Cancelled);
                    pending.response.take()
                } else if Instant::now() >= deadline {
                    pending.response = Some(ExtensionUiDialogResult::TimedOut);
                    pending.response.take()
                } else {
                    None
                }
            };
            if let Some(response) = response {
                let terminal_reason = match &response {
                    ExtensionUiDialogResult::Value(_) => ExtensionUiTerminalReason::Resolved,
                    ExtensionUiDialogResult::Cancelled => ExtensionUiTerminalReason::Cancelled,
                    ExtensionUiDialogResult::TimedOut => ExtensionUiTerminalReason::TimedOut,
                    ExtensionUiDialogResult::Error(_) => ExtensionUiTerminalReason::Failed,
                };
                state.pending.remove(id);
                // A request queued before a mode sink was installed must not
                // be emitted after the dialog has already settled.
                state
                    .outbox
                    .retain(|request| request.get("id").and_then(Value::as_str) != Some(id));
                state.remember_terminal(id.to_string(), terminal_reason);
                drop(state);
                return match response {
                    ExtensionUiDialogResult::Value(value) => {
                        Ok(ExtensionUiDialogResult::Value(value))
                    }
                    ExtensionUiDialogResult::Cancelled => Ok(ExtensionUiDialogResult::Cancelled),
                    ExtensionUiDialogResult::TimedOut => Ok(ExtensionUiDialogResult::TimedOut),
                    ExtensionUiDialogResult::Error(error) => Err(error),
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait_for = if cancel.is_some() {
                remaining.min(Duration::from_millis(25))
            } else {
                remaining
            };
            let (next, _) = self
                .core
                .wake
                .wait_timeout(state, wait_for)
                .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
            state = next;
        }
    }

    /// Resolve one response from the RPC/UI host.  Invalid responses leave a
    /// still-pending request untouched so a client can correct its envelope.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants / upstream-mirroring diagnostics
    pub fn handle_response(&self, response: &Value) -> ExtensionUiResponseDisposition {
        let Some(object) = response.as_object() else {
            return self.malformed_response(None, "response must be a JSON object");
        };
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            return self.malformed_response(None, "extension_ui_response requires a string id");
        };
        if object.get("type").and_then(Value::as_str) != Some("extension_ui_response") {
            return self
                .malformed_response(Some(id), "response type must be extension_ui_response");
        }
        let kind = {
            let Ok(state) = self.core.state.lock() else {
                return self.malformed_response(Some(id), "Extension UI broker lock poisoned");
            };
            match state.pending.get(id) {
                Some(pending) if pending.response.is_none() => Ok(pending.kind),
                Some(_) => Err((
                    ExtensionUiResponseDisposition::LateResponse,
                    "a second response arrived for an already-resolved dialog",
                    "late_response",
                )),
                None if state.terminal.get(id)
                    == Some(&ExtensionUiTerminalReason::FireAndForget) =>
                {
                    Err((
                        ExtensionUiResponseDisposition::FireAndForgetResponse,
                        "response does not belong to a dialog request",
                        "fire_and_forget_response",
                    ))
                }
                None if state.terminal.contains_key(id) => Err((
                    ExtensionUiResponseDisposition::LateResponse,
                    "response arrived after the dialog was already settled",
                    "late_response",
                )),
                None => Err((
                    ExtensionUiResponseDisposition::UnknownId,
                    "response id does not belong to a pending UI request",
                    "unknown_response_id",
                )),
            }
        };
        let kind = match kind {
            Ok(kind) => kind,
            Err((disposition, message, code)) => {
                return self.diagnose_response(disposition, message, Some(id), code);
            }
        };
        let parsed = match Self::parse_dialog_response(object, kind) {
            Ok(parsed) => parsed,
            Err(error) => return self.malformed_response(Some(id), &error),
        };
        let Ok(mut state) = self.core.state.lock() else {
            return self.malformed_response(Some(id), "Extension UI broker lock poisoned");
        };
        let disposition = match state.pending.get(id) {
            Some(pending) if pending.response.is_none() => None,
            Some(_) => Some((
                ExtensionUiResponseDisposition::LateResponse,
                "a second response arrived for an already-resolved dialog",
                "late_response",
            )),
            None if state.terminal.get(id) == Some(&ExtensionUiTerminalReason::FireAndForget) => {
                Some((
                    ExtensionUiResponseDisposition::FireAndForgetResponse,
                    "response does not belong to a dialog request",
                    "fire_and_forget_response",
                ))
            }
            None if state.terminal.contains_key(id) => Some((
                ExtensionUiResponseDisposition::LateResponse,
                "response arrived after the dialog was already settled",
                "late_response",
            )),
            None => Some((
                ExtensionUiResponseDisposition::UnknownId,
                "response id does not belong to a pending UI request",
                "unknown_response_id",
            )),
        };
        if let Some((disposition, message, code)) = disposition {
            drop(state);
            return self.diagnose_response(disposition, message, Some(id), code);
        }
        let pending = state
            .pending
            .get_mut(id)
            .expect("pending UI response was checked above");
        pending.response = Some(parsed.clone());
        if kind == ExtensionUiDialogKind::Editor {
            if let ExtensionUiDialogResult::Value(Value::String(text)) = &parsed {
                state.editor_text = text.clone();
            }
        }
        self.core.wake.notify_all();
        ExtensionUiResponseDisposition::Resolved
    }

    fn parse_dialog_response(
        object: &serde_json::Map<String, Value>,
        kind: ExtensionUiDialogKind,
    ) -> Result<ExtensionUiDialogResult, String> {
        if object.get("cancelled") == Some(&Value::Bool(true)) {
            return Ok(ExtensionUiDialogResult::Cancelled);
        }
        match kind {
            ExtensionUiDialogKind::Select
            | ExtensionUiDialogKind::Input
            | ExtensionUiDialogKind::Editor => object
                .get("value")
                .or_else(|| object.get("result"))
                .and_then(Value::as_str)
                .map(|value| ExtensionUiDialogResult::Value(Value::String(value.to_string())))
                .ok_or_else(|| {
                    "text dialog response requires a string value or result".to_string()
                }),
            ExtensionUiDialogKind::Confirm => object
                .get("confirmed")
                .or_else(|| object.get("value"))
                .or_else(|| object.get("result"))
                .and_then(Value::as_bool)
                .map(|value| ExtensionUiDialogResult::Value(Value::Bool(value)))
                .ok_or_else(|| {
                    "confirm dialog response requires a boolean confirmed, value, or result"
                        .to_string()
                }),
            ExtensionUiDialogKind::Custom => object
                .get("value")
                .or_else(|| object.get("result"))
                .cloned()
                .map(ExtensionUiDialogResult::Value)
                .ok_or_else(|| {
                    "custom UI response requires a value or result, or cancelled=true".to_string()
                }),
        }
    }

    fn malformed_response(
        &self,
        id: Option<&str>,
        message: &str,
    ) -> ExtensionUiResponseDisposition {
        self.diagnose_response(
            ExtensionUiResponseDisposition::Malformed,
            message,
            id,
            "malformed_response",
        )
    }

    fn diagnose_response(
        &self,
        disposition: ExtensionUiResponseDisposition,
        message: &str,
        id: Option<&str>,
        code: &str,
    ) -> ExtensionUiResponseDisposition {
        if let Ok(mut state) = self.core.state.lock() {
            state.diagnostic(code, id.map(ToOwned::to_owned), message);
        }
        disposition
    }

    fn fire_and_forget(
        &self,
        method: &str,
        mut request: serde_json::Map<String, Value>,
    ) -> Result<(), String> {
        self.ensure_enabled()?;
        let id = self.next_id();
        request.insert(
            "type".to_string(),
            Value::String("extension_ui_request".to_string()),
        );
        request.insert("id".to_string(), Value::String(id.clone()));
        let request = Value::Object(request);
        let sink = {
            let mut state = self
                .core
                .state
                .lock()
                .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
            if !state.enabled {
                return Err(
                    "Extension UI is not available in this mode or during startup".to_string(),
                );
            }
            state.remember_terminal(id, ExtensionUiTerminalReason::FireAndForget);
            if let Some(sink) = state.sink.clone() {
                Some(sink)
            } else {
                state.outbox.push_back(request.clone());
                None
            }
        };
        if let Some(sink) = sink {
            sink(request).map_err(|error| format!("{method} request delivery failed: {error}"))?;
        }
        Ok(())
    }

    pub fn notify(&self, message: &str, notify_type: Option<&str>) -> Result<(), String> {
        if let Some(notify_type) = notify_type {
            if !matches!(notify_type, "info" | "warning" | "error") {
                return Err(format!("invalid notification type: {notify_type}"));
            }
        }
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("notify".to_string()));
        request.insert("message".to_string(), Value::String(message.to_string()));
        if let Some(notify_type) = notify_type {
            request.insert(
                "notifyType".to_string(),
                Value::String(notify_type.to_string()),
            );
        }
        self.fire_and_forget("notify", request)
    }

    pub fn set_status(&self, key: &str, text: Option<&str>) -> Result<(), String> {
        let dispatch = {
            let mut state = self
                .core
                .state
                .lock()
                .map_err(|_| "Extension UI broker lock poisoned".to_string())?;
            if let Some(text) = text {
                state
                    .extension_statuses
                    .insert(key.to_string(), text.to_string());
            } else {
                state.extension_statuses.remove(key);
            }

            if !state.enabled {
                None
            } else {
                let id = self.next_id();
                let mut request = serde_json::Map::new();
                request.insert("method".to_string(), Value::String("setStatus".to_string()));
                request.insert("statusKey".to_string(), Value::String(key.to_string()));
                if let Some(text) = text {
                    request.insert("statusText".to_string(), Value::String(text.to_string()));
                }
                request.insert(
                    "type".to_string(),
                    Value::String("extension_ui_request".to_string()),
                );
                request.insert("id".to_string(), Value::String(id.clone()));
                let request = Value::Object(request);
                state.remember_terminal(id, ExtensionUiTerminalReason::FireAndForget);
                if let Some(sink) = state.sink.clone() {
                    Some((sink, request))
                } else {
                    state.outbox.push_back(request);
                    None
                }
            }
        };
        self.core.status_wakeup.notify_one();
        if let Some((sink, request)) = dispatch {
            sink(request).map_err(|error| format!("setStatus request delivery failed: {error}"))?;
        }
        Ok(())
    }

    pub fn set_working_message(&self, message: Option<&str>) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("setWorkingMessage".to_string()),
        );
        if let Some(message) = message {
            request.insert(
                "workingMessage".to_string(),
                Value::String(message.to_string()),
            );
        }
        self.fire_and_forget("setWorkingMessage", request)
    }

    pub fn set_working_visible(&self, visible: bool) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("setWorkingVisible".to_string()),
        );
        request.insert("workingVisible".to_string(), Value::Bool(visible));
        self.fire_and_forget("setWorkingVisible", request)
    }

    pub fn set_working_indicator(&self, options: Option<Value>) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("setWorkingIndicator".to_string()),
        );
        if let Some(options) = options {
            request.insert("workingIndicator".to_string(), options);
        }
        self.fire_and_forget("setWorkingIndicator", request)
    }

    pub fn set_widget(
        &self,
        key: &str,
        lines: Option<&[String]>,
        placement: Option<&str>,
    ) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("setWidget".to_string()));
        request.insert("widgetKey".to_string(), Value::String(key.to_string()));
        let widget_state = lines.map(|lines| {
            serde_json::json!({
                "lines": lines,
                "placement": placement,
            })
        });
        if let Some(lines) = lines {
            request.insert(
                "widgetLines".to_string(),
                Value::Array(lines.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(placement) = placement {
            request.insert(
                "widgetPlacement".to_string(),
                Value::String(placement.to_string()),
            );
        }
        let result = self.fire_and_forget("setWidget", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                if let Some(widget_state) = widget_state {
                    state.widgets.insert(key.to_string(), widget_state);
                } else {
                    state.widgets.remove(key);
                }
            }
        }
        result
    }

    pub fn set_widget_factory(
        &self,
        key: &str,
        factory: Option<ExtensionUiFactory>,
        placement: Option<&str>,
    ) -> Result<(), String> {
        let rendered = factory
            .map(|factory| {
                self.materialize_factory(
                    factory,
                    "widget",
                    serde_json::json!({
                        "key": key,
                        "placement": placement,
                    }),
                )
            })
            .transpose()?;
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("setWidget".to_string()));
        request.insert("widgetKey".to_string(), Value::String(key.to_string()));
        request.insert(
            "widgetFactory".to_string(),
            rendered.clone().unwrap_or(Value::Null),
        );
        if let Some(placement) = placement {
            request.insert(
                "widgetPlacement".to_string(),
                Value::String(placement.to_string()),
            );
        }
        let result = self.fire_and_forget("setWidget", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                if let Some(rendered) = rendered {
                    state.widgets.insert(key.to_string(), rendered);
                } else {
                    state.widgets.remove(key);
                }
            }
        }
        result
    }

    pub fn set_hidden_thinking_label(&self, label: Option<&str>) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("setHiddenThinkingLabel".to_string()),
        );
        request.insert(
            "label".to_string(),
            label.map_or(Value::Null, |label| Value::String(label.to_string())),
        );
        let result = self.fire_and_forget("setHiddenThinkingLabel", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.hidden_thinking_label = label.map(ToOwned::to_owned);
            }
        }
        result
    }

    pub fn hidden_thinking_label(&self) -> Option<String> {
        self.core
            .state
            .lock()
            .ok()
            .and_then(|state| state.hidden_thinking_label.clone())
    }

    pub fn set_header(&self, factory: Option<ExtensionUiFactory>) -> Result<(), String> {
        let rendered = factory
            .map(|factory| self.materialize_factory(factory, "header", Value::Null))
            .transpose()?;
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("setHeader".to_string()));
        request.insert(
            "headerFactory".to_string(),
            rendered.clone().unwrap_or(Value::Null),
        );
        let result = self.fire_and_forget("setHeader", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.header = rendered;
            }
        }
        result
    }

    pub fn set_footer(&self, factory: Option<ExtensionUiFactory>) -> Result<(), String> {
        let rendered = factory
            .map(|factory| self.materialize_factory(factory, "footer", Value::Null))
            .transpose()?;
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("setFooter".to_string()));
        request.insert(
            "footerFactory".to_string(),
            rendered.clone().unwrap_or(Value::Null),
        );
        let result = self.fire_and_forget("setFooter", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.footer = rendered;
            }
        }
        result
    }

    pub fn add_autocomplete_provider(&self, factory: ExtensionUiFactory) -> Result<(), String> {
        let rendered = self.materialize_factory(factory, "autocomplete", Value::Null)?;
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("addAutocompleteProvider".to_string()),
        );
        request.insert("autocompleteFactory".to_string(), rendered.clone());
        let result = self.fire_and_forget("addAutocompleteProvider", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.autocomplete_providers.push(rendered);
            }
        }
        result
    }

    pub fn autocomplete_providers(&self) -> Vec<Value> {
        self.core
            .state
            .lock()
            .map(|state| state.autocomplete_providers.clone())
            .unwrap_or_default()
    }

    pub fn set_editor_component(&self, factory: Option<ExtensionUiFactory>) -> Result<(), String> {
        let rendered = factory
            .map(|factory| self.materialize_factory(factory, "editor", Value::Null))
            .transpose()?;
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("setEditorComponent".to_string()),
        );
        request.insert(
            "editorComponent".to_string(),
            rendered.clone().unwrap_or(Value::Null),
        );
        let result = self.fire_and_forget("setEditorComponent", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.editor_component = rendered;
            }
        }
        result
    }

    pub fn editor_component(&self) -> Option<Value> {
        self.core
            .state
            .lock()
            .ok()
            .and_then(|state| state.editor_component.clone())
    }

    pub fn set_themes(&self, themes: Vec<Value>) {
        if let Ok(mut state) = self.core.state.lock() {
            state.themes = themes;
        }
    }

    pub fn get_all_themes(&self) -> Vec<Value> {
        self.core
            .state
            .lock()
            .map(|state| state.themes.clone())
            .unwrap_or_default()
    }

    pub fn get_theme(&self, name: &str) -> Option<Value> {
        self.core.state.lock().ok().and_then(|state| {
            state.themes.iter().find_map(|theme| {
                if theme
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|theme_name| theme_name == name)
                    || theme.as_str() == Some(name)
                {
                    Some(theme.clone())
                } else {
                    None
                }
            })
        })
    }

    pub fn theme(&self) -> Option<Value> {
        self.core
            .state
            .lock()
            .ok()
            .and_then(|state| state.current_theme.clone())
    }

    /// Set a theme by a name known to the theme catalog or by an open-JSON
    /// theme object. The returned value preserves upstream success/error
    /// semantics; transport failure is returned as Rust Err.
    pub fn set_theme(&self, theme: Value) -> Result<ExtensionUiThemeResult, String> {
        let selected = if let Some(name) = theme.as_str() {
            let Some(selected) = self.get_theme(name) else {
                return Ok(ExtensionUiThemeResult::failure(format!(
                    "Theme not found: {name}"
                )));
            };
            selected
        } else if theme.is_object() {
            theme.clone()
        } else {
            return Ok(ExtensionUiThemeResult::failure(
                "theme must be a theme name or object",
            ));
        };
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("setTheme".to_string()));
        request.insert("theme".to_string(), theme);
        self.fire_and_forget("setTheme", request)?;
        if let Ok(mut state) = self.core.state.lock() {
            state.current_theme = Some(selected);
        }
        Ok(ExtensionUiThemeResult::success())
    }

    pub fn set_current_theme(&self, theme: Option<Value>) {
        if let Ok(mut state) = self.core.state.lock() {
            state.current_theme = theme;
        }
    }

    pub fn get_tools_expanded(&self) -> bool {
        self.core
            .state
            .lock()
            .map(|state| state.tools_expanded)
            .unwrap_or(false)
    }

    pub fn set_tools_expanded(&self, expanded: bool) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("setToolsExpanded".to_string()),
        );
        request.insert("expanded".to_string(), Value::Bool(expanded));
        let result = self.fire_and_forget("setToolsExpanded", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.tools_expanded = expanded;
            }
        }
        result
    }

    pub fn set_title(&self, title: &str) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("setTitle".to_string()));
        request.insert("title".to_string(), Value::String(title.to_string()));
        self.fire_and_forget("setTitle", request)
    }

    pub fn set_editor_text(&self, text: &str) -> Result<(), String> {
        let mut request = serde_json::Map::new();
        request.insert(
            "method".to_string(),
            Value::String("set_editor_text".to_string()),
        );
        request.insert("text".to_string(), Value::String(text.to_string()));
        let result = self.fire_and_forget("set_editor_text", request);
        if result.is_ok() {
            if let Ok(mut state) = self.core.state.lock() {
                state.editor_text = text.to_string();
            }
        }
        result
    }

    pub fn paste_to_editor(&self, text: &str) -> Result<(), String> {
        // RPC clients have no terminal paste decoder. Keeping the upstream
        // fallback to set_editor_text preserves the editor's observable text
        // while retaining a fire-and-forget wire request.
        self.set_editor_text(text)
    }

    pub fn editor_text(&self) -> String {
        self.core
            .state
            .lock()
            .map(|state| state.editor_text.clone())
            .unwrap_or_default()
    }

    pub fn select(
        &self,
        title: &str,
        options: &[String],
        options_config: ExtensionUiDialogOptions,
        blocking_allowed: bool,
    ) -> Result<Option<String>, String> {
        if !blocking_allowed {
            return Err(BLOCKING_UI_UNAVAILABLE_MESSAGE.to_string());
        }
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("select".to_string()));
        request.insert("title".to_string(), Value::String(title.to_string()));
        request.insert(
            "options".to_string(),
            Value::Array(options.iter().cloned().map(Value::String).collect()),
        );
        if let Some(timeout) = options_config.timeout {
            request.insert(
                "timeout".to_string(),
                Value::from(Self::bounded_timeout(Some(timeout)).as_millis() as u64),
            );
        }
        match self.submit_dialog(request, ExtensionUiDialogKind::Select, options_config)? {
            ExtensionUiDialogResult::Value(Value::String(value)) => Ok(Some(value)),
            ExtensionUiDialogResult::Value(_)
            | ExtensionUiDialogResult::Cancelled
            | ExtensionUiDialogResult::TimedOut => Ok(None),
            ExtensionUiDialogResult::Error(error) => Err(error),
        }
    }

    pub fn confirm(
        &self,
        title: &str,
        message: &str,
        options_config: ExtensionUiDialogOptions,
        blocking_allowed: bool,
    ) -> Result<bool, String> {
        if !blocking_allowed {
            return Err(BLOCKING_UI_UNAVAILABLE_MESSAGE.to_string());
        }
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("confirm".to_string()));
        request.insert("title".to_string(), Value::String(title.to_string()));
        request.insert("message".to_string(), Value::String(message.to_string()));
        if let Some(timeout) = options_config.timeout {
            request.insert(
                "timeout".to_string(),
                Value::from(Self::bounded_timeout(Some(timeout)).as_millis() as u64),
            );
        }
        match self.submit_dialog(request, ExtensionUiDialogKind::Confirm, options_config)? {
            ExtensionUiDialogResult::Value(Value::Bool(value)) => Ok(value),
            ExtensionUiDialogResult::Value(_)
            | ExtensionUiDialogResult::Cancelled
            | ExtensionUiDialogResult::TimedOut => Ok(false),
            ExtensionUiDialogResult::Error(error) => Err(error),
        }
    }

    pub fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        options_config: ExtensionUiDialogOptions,
        blocking_allowed: bool,
    ) -> Result<Option<String>, String> {
        if !blocking_allowed {
            return Err(BLOCKING_UI_UNAVAILABLE_MESSAGE.to_string());
        }
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("input".to_string()));
        request.insert("title".to_string(), Value::String(title.to_string()));
        if let Some(placeholder) = placeholder {
            request.insert(
                "placeholder".to_string(),
                Value::String(placeholder.to_string()),
            );
        }
        if let Some(timeout) = options_config.timeout {
            request.insert(
                "timeout".to_string(),
                Value::from(Self::bounded_timeout(Some(timeout)).as_millis() as u64),
            );
        }
        match self.submit_dialog(request, ExtensionUiDialogKind::Input, options_config)? {
            ExtensionUiDialogResult::Value(Value::String(value)) => Ok(Some(value)),
            ExtensionUiDialogResult::Value(_)
            | ExtensionUiDialogResult::Cancelled
            | ExtensionUiDialogResult::TimedOut => Ok(None),
            ExtensionUiDialogResult::Error(error) => Err(error),
        }
    }

    pub fn editor(
        &self,
        title: &str,
        prefill: Option<&str>,
        options_config: ExtensionUiDialogOptions,
        blocking_allowed: bool,
    ) -> Result<Option<String>, String> {
        if !blocking_allowed {
            return Err(BLOCKING_UI_UNAVAILABLE_MESSAGE.to_string());
        }
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("editor".to_string()));
        request.insert("title".to_string(), Value::String(title.to_string()));
        if let Some(prefill) = prefill {
            request.insert("prefill".to_string(), Value::String(prefill.to_string()));
        }
        // The upstream editor has no timeout field; the broker still applies
        // the bounded Rust wait from `options_config`.
        match self.submit_dialog(request, ExtensionUiDialogKind::Editor, options_config)? {
            ExtensionUiDialogResult::Value(Value::String(value)) => Ok(Some(value)),
            ExtensionUiDialogResult::Value(_)
            | ExtensionUiDialogResult::Cancelled
            | ExtensionUiDialogResult::TimedOut => Ok(None),
            ExtensionUiDialogResult::Error(error) => Err(error),
        }
    }

    /// Request a custom host overlay. The factory is materialized into an
    /// open-JSON component description before the correlated request is sent;
    /// the host response may contain any JSON value and is returned to the
    /// native callback.
    pub fn custom(
        &self,
        factory: ExtensionUiFactory,
        options: Option<Value>,
        options_config: ExtensionUiDialogOptions,
        blocking_allowed: bool,
    ) -> Result<Option<Value>, String> {
        if !blocking_allowed {
            return Err(BLOCKING_UI_UNAVAILABLE_MESSAGE.to_string());
        }
        let rendered =
            self.materialize_factory(factory, "custom", options.clone().unwrap_or(Value::Null))?;
        let mut request = serde_json::Map::new();
        request.insert("method".to_string(), Value::String("custom".to_string()));
        request.insert("factory".to_string(), rendered);
        request.insert("options".to_string(), options.unwrap_or(Value::Null));
        if let Some(timeout) = options_config.timeout {
            request.insert(
                "timeout".to_string(),
                Value::from(Self::bounded_timeout(Some(timeout)).as_millis() as u64),
            );
        }
        match self.submit_dialog(request, ExtensionUiDialogKind::Custom, options_config)? {
            ExtensionUiDialogResult::Value(value) => Ok(Some(value)),
            ExtensionUiDialogResult::Cancelled | ExtensionUiDialogResult::TimedOut => Ok(None),
            ExtensionUiDialogResult::Error(error) => Err(error),
        }
    }
}

/// Context exposed to native extension handlers. Fire-and-forget methods are
/// safe in every synchronous callback. Dialog and custom-overlay methods are
/// blocking and return `BLOCKING_UI_UNAVAILABLE_MESSAGE` unless the runner
/// explicitly marked the callback as worker-safe. Terminal-input listeners,
/// renderer-neutral component factories, header/footer state, themes, and
/// tool-expansion state are all broker-owned and exposed to the host through
/// the same native/RPC boundary.
#[derive(Clone, Default)]
pub struct ExtensionUiContext {
    broker: ExtensionUiBroker,
    enabled: bool,
    blocking_allowed: bool,
    active: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for ExtensionUiContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionUiContext")
            .field("enabled", &self.enabled)
            .field("blocking_allowed", &self.blocking_allowed)
            .field(
                "active",
                &self
                    .active
                    .as_ref()
                    .map(|active| active.load(Ordering::Acquire)),
            )
            .finish()
    }
}

impl ExtensionUiContext {
    #[cfg(test)]
    pub(crate) fn new(broker: ExtensionUiBroker, enabled: bool, blocking_allowed: bool) -> Self {
        Self::new_with_active(broker, enabled, blocking_allowed, None)
    }

    fn new_with_active(
        broker: ExtensionUiBroker,
        enabled: bool,
        blocking_allowed: bool,
        active: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            broker,
            enabled,
            blocking_allowed,
            active,
        }
    }

    pub fn broker(&self) -> ExtensionUiBroker {
        self.broker.clone()
    }

    fn ensure_active(&self) -> Result<(), String> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.load(Ordering::Acquire))
        {
            Err(STALE_MESSAGE.to_string())
        } else {
            Ok(())
        }
    }

    fn ensure_fire_and_forget(&self) -> Result<(), String> {
        self.ensure_active()?;
        if self.enabled {
            self.broker.ensure_enabled()
        } else {
            Err("Extension UI is not available in this mode".to_string())
        }
    }

    pub fn select(
        &self,
        title: &str,
        options: &[String],
        timeout: Option<Duration>,
    ) -> Result<Option<String>, String> {
        self.select_with_options(
            title,
            options,
            ExtensionUiDialogOptions {
                timeout,
                cancel: None,
            },
        )
    }

    pub fn select_with_options(
        &self,
        title: &str,
        options: &[String],
        options_config: ExtensionUiDialogOptions,
    ) -> Result<Option<String>, String> {
        self.ensure_active()?;
        self.broker.select(
            title,
            options,
            options_config,
            self.enabled && self.blocking_allowed,
        )
    }

    pub fn confirm(
        &self,
        title: &str,
        message: &str,
        timeout: Option<Duration>,
    ) -> Result<bool, String> {
        self.confirm_with_options(
            title,
            message,
            ExtensionUiDialogOptions {
                timeout,
                cancel: None,
            },
        )
    }

    pub fn confirm_with_options(
        &self,
        title: &str,
        message: &str,
        options_config: ExtensionUiDialogOptions,
    ) -> Result<bool, String> {
        self.ensure_active()?;
        self.broker.confirm(
            title,
            message,
            options_config,
            self.enabled && self.blocking_allowed,
        )
    }

    pub fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<Option<String>, String> {
        self.input_with_options(
            title,
            placeholder,
            ExtensionUiDialogOptions {
                timeout,
                cancel: None,
            },
        )
    }

    pub fn input_with_options(
        &self,
        title: &str,
        placeholder: Option<&str>,
        options_config: ExtensionUiDialogOptions,
    ) -> Result<Option<String>, String> {
        self.ensure_active()?;
        self.broker.input(
            title,
            placeholder,
            options_config,
            self.enabled && self.blocking_allowed,
        )
    }

    pub fn editor(&self, title: &str, prefill: Option<&str>) -> Result<Option<String>, String> {
        self.editor_with_options(title, prefill, ExtensionUiDialogOptions::default())
    }

    pub fn editor_with_options(
        &self,
        title: &str,
        prefill: Option<&str>,
        options_config: ExtensionUiDialogOptions,
    ) -> Result<Option<String>, String> {
        self.ensure_active()?;
        self.broker.editor(
            title,
            prefill,
            options_config,
            self.enabled && self.blocking_allowed,
        )
    }

    pub fn custom(
        &self,
        factory: ExtensionUiFactory,
        options: Option<Value>,
    ) -> Result<Option<Value>, String> {
        self.custom_with_options(factory, options, ExtensionUiDialogOptions::default())
    }

    pub fn custom_with_options(
        &self,
        factory: ExtensionUiFactory,
        options: Option<Value>,
        options_config: ExtensionUiDialogOptions,
    ) -> Result<Option<Value>, String> {
        self.ensure_active()?;
        self.broker.custom(
            factory,
            options,
            options_config,
            self.enabled && self.blocking_allowed,
        )
    }

    pub fn notify(&self, message: &str, notify_type: Option<&str>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.notify(message, notify_type)
    }

    pub fn set_status(&self, key: &str, text: Option<&str>) -> Result<(), String> {
        self.ensure_active()?;
        if !self.enabled {
            return Err("Extension UI is not available in this mode".to_string());
        }
        self.broker.set_status(key, text)
    }

    pub fn set_working_message(&self, message: Option<&str>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_working_message(message)
    }

    pub fn set_working(&self, message: Option<&str>) -> Result<(), String> {
        self.set_working_message(message)
    }

    pub fn set_working_visible(&self, visible: bool) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_working_visible(visible)
    }

    pub fn set_working_indicator(&self, options: Option<Value>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_working_indicator(options)
    }

    pub fn set_widget(
        &self,
        key: &str,
        lines: Option<&[String]>,
        placement: Option<&str>,
    ) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_widget(key, lines, placement)
    }

    pub fn set_widget_factory(
        &self,
        key: &str,
        factory: Option<ExtensionUiFactory>,
        placement: Option<&str>,
    ) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_widget_factory(key, factory, placement)
    }

    pub fn set_hidden_thinking_label(&self, label: Option<&str>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_hidden_thinking_label(label)
    }

    pub fn get_hidden_thinking_label(&self) -> Option<String> {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.hidden_thinking_label()
        } else {
            None
        }
    }

    pub fn set_header(&self, factory: Option<ExtensionUiFactory>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_header(factory)
    }

    pub fn set_footer(&self, factory: Option<ExtensionUiFactory>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_footer(factory)
    }

    pub fn add_autocomplete_provider(&self, factory: ExtensionUiFactory) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.add_autocomplete_provider(factory)
    }

    pub fn get_autocomplete_providers(&self) -> Vec<Value> {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.autocomplete_providers()
        } else {
            Vec::new()
        }
    }

    pub fn set_editor_component(&self, factory: Option<ExtensionUiFactory>) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_editor_component(factory)
    }

    pub fn get_editor_component(&self) -> Option<Value> {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.editor_component()
        } else {
            None
        }
    }

    pub fn on_terminal_input(
        &self,
        handler: TerminalInputHandler,
    ) -> Result<ExtensionUiSubscription, String> {
        self.ensure_active()?;
        if !self.enabled {
            return Err("Extension UI is not available in this mode".to_string());
        }
        self.broker.add_terminal_input_listener(handler)
    }

    pub fn add_terminal_input_listener(
        &self,
        handler: TerminalInputHandler,
    ) -> Result<ExtensionUiSubscription, String> {
        self.on_terminal_input(handler)
    }

    pub fn theme(&self) -> Option<Value> {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.theme()
        } else {
            None
        }
    }

    pub fn get_all_themes(&self) -> Vec<Value> {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.get_all_themes()
        } else {
            Vec::new()
        }
    }

    pub fn get_theme(&self, name: &str) -> Option<Value> {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.get_theme(name)
        } else {
            None
        }
    }

    pub fn set_theme(&self, theme: Value) -> Result<ExtensionUiThemeResult, String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_theme(theme)
    }

    pub fn get_tools_expanded(&self) -> bool {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.get_tools_expanded()
        } else {
            false
        }
    }

    pub fn set_tools_expanded(&self, expanded: bool) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_tools_expanded(expanded)
    }

    pub fn ui_state_snapshot(&self) -> Value {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.ui_state_snapshot()
        } else {
            serde_json::json!({})
        }
    }

    pub fn set_title(&self, title: &str) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_title(title)
    }

    pub fn paste_to_editor(&self, text: &str) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.paste_to_editor(text)
    }

    pub fn set_editor_text(&self, text: &str) -> Result<(), String> {
        self.ensure_fire_and_forget()?;
        self.broker.set_editor_text(text)
    }

    pub fn get_editor_text(&self) -> String {
        if self.enabled
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
        {
            self.broker.editor_text()
        } else {
            String::new()
        }
    }
}

pub const BLOCKING_UI_UNAVAILABLE_MESSAGE: &str = "blocking extension UI is only available from a worker-safe extension callback; synchronous dispatch would deadlock the RPC input loop";

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Renderer closures are the Rust-native equivalent of extension renderCall,
/// renderResult, and renderEntry callbacks. Their JSON payloads retain the
/// upstream open shape while their callback boundary is panic-safe in the
/// runner.
pub type MessageRenderer =
    Arc<dyn Fn(&Value, &Value) -> Result<Option<Value>, String> + Send + Sync>;
pub type RendererFn = MessageRenderer;
pub type EntryRenderer = Arc<dyn Fn(&Value, &Value) -> Result<Option<Value>, String> + Send + Sync>;
pub type MarkdownTransformer =
    Arc<dyn Fn(&str, &MarkdownTransformContext) -> Result<String, String> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkdownTransformContext {
    pub message_type: String,
    pub is_streaming: bool,
    pub available_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationKind {
    Handler,
    Tool,
    Command,
    Shortcut,
    Flag,
    MessageRenderer,
    MarkdownTransformer,
    EntryRenderer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationRecord {
    pub kind: RegistrationKind,
    pub name: Option<String>,
}

/// A host action that can be dispatched from a running extension callback.
///
/// Mode adapters carry the arguments as JSON at the callback boundary.
/// Keeping the action names typed on the Rust side prevents an unknown host
/// method from being silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionHostAction {
    WaitForIdle,
    NewSession,
    Fork,
    NavigateTree,
    SwitchSession,
    Reload,
    SendMessage,
    SendUserMessage,
    AppendEntry,
    SetSessionName,
    GetSessionName,
    SetLabel,
    GetActiveTools,
    GetAllTools,
    SetActiveTools,
    GetCommands,
    SetModel,
    GetThinkingLevel,
    SetThinkingLevel,
    GetModel,
    GetScopedModels,
    IsIdle,
    IsProjectTrusted,
    GetSignal,
    Abort,
    HasPendingMessages,
    Shutdown,
    GetContextUsage,
    Compact,
    GetSystemPrompt,
    GetSystemPromptOptions,
    ToolUpdate,
}

impl ExtensionHostAction {
    pub fn from_protocol_name(name: &str) -> Option<Self> {
        Some(match name {
            "waitForIdle" => Self::WaitForIdle,
            "newSession" => Self::NewSession,
            "fork" => Self::Fork,
            "navigateTree" => Self::NavigateTree,
            "switchSession" => Self::SwitchSession,
            "reload" => Self::Reload,
            "sendMessage" => Self::SendMessage,
            "sendUserMessage" => Self::SendUserMessage,
            "appendEntry" => Self::AppendEntry,
            "setSessionName" => Self::SetSessionName,
            "getSessionName" => Self::GetSessionName,
            "setLabel" => Self::SetLabel,
            "getActiveTools" => Self::GetActiveTools,
            "getAllTools" => Self::GetAllTools,
            "setActiveTools" => Self::SetActiveTools,
            "getCommands" => Self::GetCommands,
            "setModel" => Self::SetModel,
            "getThinkingLevel" => Self::GetThinkingLevel,
            "setThinkingLevel" => Self::SetThinkingLevel,
            "getModel" => Self::GetModel,
            "getScopedModels" => Self::GetScopedModels,
            "isIdle" => Self::IsIdle,
            "isProjectTrusted" => Self::IsProjectTrusted,
            "getSignal" => Self::GetSignal,
            "abort" => Self::Abort,
            "hasPendingMessages" => Self::HasPendingMessages,
            "shutdown" => Self::Shutdown,
            "getContextUsage" => Self::GetContextUsage,
            "compact" => Self::Compact,
            "getSystemPrompt" => Self::GetSystemPrompt,
            "getSystemPromptOptions" => Self::GetSystemPromptOptions,
            "toolUpdate" => Self::ToolUpdate,
            _ => return None,
        })
    }

    /// Return the external bridge spelling for this host action.
    pub const fn protocol_name(self) -> &'static str {
        match self {
            Self::WaitForIdle => "waitForIdle",
            Self::NewSession => "newSession",
            Self::Fork => "fork",
            Self::NavigateTree => "navigateTree",
            Self::SwitchSession => "switchSession",
            Self::Reload => "reload",
            Self::SendMessage => "sendMessage",
            Self::SendUserMessage => "sendUserMessage",
            Self::AppendEntry => "appendEntry",
            Self::SetSessionName => "setSessionName",
            Self::GetSessionName => "getSessionName",
            Self::SetLabel => "setLabel",
            Self::GetActiveTools => "getActiveTools",
            Self::GetAllTools => "getAllTools",
            Self::SetActiveTools => "setActiveTools",
            Self::GetCommands => "getCommands",
            Self::SetModel => "setModel",
            Self::GetThinkingLevel => "getThinkingLevel",
            Self::SetThinkingLevel => "setThinkingLevel",
            Self::GetModel => "getModel",
            Self::GetScopedModels => "getScopedModels",
            Self::IsIdle => "isIdle",
            Self::IsProjectTrusted => "isProjectTrusted",
            Self::GetSignal => "getSignal",
            Self::Abort => "abort",
            Self::HasPendingMessages => "hasPendingMessages",
            Self::Shutdown => "shutdown",
            Self::GetContextUsage => "getContextUsage",
            Self::Compact => "compact",
            Self::GetSystemPrompt => "getSystemPrompt",
            Self::GetSystemPromptOptions => "getSystemPromptOptions",
            Self::ToolUpdate => "toolUpdate",
        }
    }

    pub const fn is_lifecycle(self) -> bool {
        matches!(
            self,
            Self::NewSession | Self::Fork | Self::NavigateTree | Self::SwitchSession | Self::Reload
        )
    }
}

/// A host request that has been accepted but still needs mode/loader work.
///
/// `args` retains the original bridge arguments, while `payload` is the
/// normalized mode-facing action already used by the legacy drain API. Keeping
/// both preserves the typed operation and the original request data for the
/// loader completion response; this type deliberately does not claim that the
/// operation has completed.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingHostAction {
    pub action: ExtensionHostAction,
    pub args: Value,
    pub payload: Value,
    /// Optional bridge-owned continuation data copied from the reserved
    /// `args.options.__bridgeContinuation` field. The host never interprets
    /// it, and the original marker remains in `args` and `payload`.
    pub continuation: Option<Value>,
}

impl PendingHostAction {
    pub fn new(action: ExtensionHostAction, args: Value, payload: Value) -> Self {
        Self::with_continuation(action, args, payload, None)
    }

    pub fn with_continuation(
        action: ExtensionHostAction,
        args: Value,
        payload: Value,
        continuation: Option<Value>,
    ) -> Self {
        let continuation = continuation.or_else(|| {
            args.get("options")
                .and_then(|options| options.get("__bridgeContinuation"))
                .cloned()
                .or_else(|| args.get("__bridgeContinuation").cloned())
        });
        Self {
            action,
            args,
            payload,
            continuation,
        }
    }

    pub fn is_lifecycle(&self) -> bool {
        self.action.is_lifecycle()
    }

    pub fn continuation_metadata(&self) -> Option<&Value> {
        self.continuation.as_ref()
    }
}

/// Optional sink used by a loader to emit the completion for a lifecycle
/// request after the mode has actually applied it.
pub type LifecycleCompletionSink =
    Arc<dyn Fn(PendingHostAction, Value) -> Result<(), String> + Send + Sync + 'static>;

/// Host changes requested by an extension and waiting for mode application.
/// Each field is optional because a drain consumes only the latest request of
/// that kind; no option means that no request is pending.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RequestedHostChanges {
    pub model: Option<Value>,
    pub model_request: Option<PendingHostAction>,
    pub active_tools: Option<Vec<String>>,
}

/// Result of the explicit host-state dispatch API. `Completed` means the
/// callback's synchronous host call was accepted; for upstream void actions
/// such as `compact` and `shutdown`, the mode operation may still be queued.
/// `Pending` is reserved for promise-like session/model operations whose
/// completion must be emitted after the mode applies them.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionHostActionOutcome {
    Completed(Value),
    Pending(PendingHostAction),
}

/// Rust integration point for actions that are owned by the coding-agent
/// host rather than by the extension runtime itself. Implementations should
/// not re-enter the same extension callback while dispatching an action.
pub trait ExtensionHostActions: Send + Sync {
    fn dispatch(&self, action: ExtensionHostAction, args: &Value) -> Result<Value, String>;

    /// Return the mode-owned UI broker. Hosts opt in by returning their live
    /// broker; the compatibility default represents a host with no UI
    /// transport and therefore cannot accidentally block a callback.
    fn ui_broker(&self) -> ExtensionUiBroker {
        ExtensionUiBroker::disabled()
    }

    /// Dispatch while preserving whether the host merely queued the request.
    /// Existing implementations remain compatible through the default, while
    /// the loader integration can retain `Pending` instead of fabricating
    /// completion.
    fn dispatch_with_outcome(
        &self,
        action: ExtensionHostAction,
        args: &Value,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch(action, args)
            .map(ExtensionHostActionOutcome::Completed)
    }

    /// Install the optional lifecycle completion sink. Hosts that do not
    /// support completion yet may keep the default no-op implementation.
    fn set_lifecycle_completion_sink(&self, _sink: LifecycleCompletionSink) {}

    /// Return the synchronous host state visible to an extension callback.
    ///
    /// Synchronous getters use a point-in-time snapshot supplied by the host
    /// callback boundary. Implementations may return an empty object when a
    /// field has no value; callers then use the upstream-shaped empty default.
    fn snapshot(&self) -> Value {
        Value::Object(Default::default())
    }

    /// Return a callback-scoped snapshot. The default preserves the original
    /// host snapshot contract; hosts that execute parallel tools can use the
    /// request's tool-call id to select the matching signal state.
    fn snapshot_for(&self, request: &Value) -> Value {
        let _ = request;
        self.snapshot()
    }
}

/// Host capabilities made available to a native extension callback.
///
/// The upstream context stores concrete `sessionManager` and `modelRegistry`
/// objects. Rust keeps the ownership boundary in the coding-agent mode, so a
/// callback receives this cloneable host handle instead. Every method below
/// goes through the bound `ExtensionHostActions` implementation; it is not a
/// snapshot-only facade and it never manufactures completion for a queued
/// mode action. `session_manager()` and `model_registry()` return capability
/// views of this same handle for callers porting code that used those
/// upstream objects.
#[derive(Clone, Default)]
pub struct ExtensionHostContext {
    actions: Option<Arc<dyn ExtensionHostActions>>,
    blocking_allowed: bool,
    tool_call_id: Option<String>,
    active: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for ExtensionHostContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionHostContext")
            .field("bound", &self.actions.is_some())
            .field("blocking_allowed", &self.blocking_allowed)
            .field("tool_call_id", &self.tool_call_id)
            .finish()
    }
}

impl ExtensionHostContext {
    pub(crate) fn new(
        actions: Option<Arc<dyn ExtensionHostActions>>,
        blocking_allowed: bool,
        tool_call_id: Option<&str>,
        active: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            actions,
            blocking_allowed,
            tool_call_id: tool_call_id.map(ToOwned::to_owned),
            active,
        }
    }

    pub fn is_bound(&self) -> bool {
        self.actions.is_some()
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
    }

    pub fn blocking_allowed(&self) -> bool {
        self.blocking_allowed
    }

    fn actions(&self) -> Result<&Arc<dyn ExtensionHostActions>, String> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.load(Ordering::Acquire))
        {
            return Err(STALE_MESSAGE.to_string());
        }
        self.actions.as_ref().ok_or_else(|| {
            format!("{NOT_INITIALIZED_MESSAGE}: native extension host actions are not bound")
        })
    }

    fn require_blocking(&self, action: ExtensionHostAction) -> Result<(), String> {
        if self.blocking_allowed {
            Ok(())
        } else {
            Err(format!(
                "blocking host action '{}' is only available from a worker-safe extension callback; synchronous dispatch would deadlock the RPC input loop",
                action.protocol_name()
            ))
        }
    }

    fn args_with_tool_call_id(&self, mut args: serde_json::Map<String, Value>) -> Value {
        if let Some(tool_call_id) = &self.tool_call_id {
            args.entry("toolCallId".to_string())
                .or_insert_with(|| Value::String(tool_call_id.clone()));
        }
        Value::Object(args)
    }

    fn dispatch_with_outcome(
        &self,
        action: ExtensionHostAction,
        args: serde_json::Map<String, Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        let args = self.args_with_tool_call_id(args);
        self.actions()?.dispatch_with_outcome(action, &args)
    }

    fn dispatch_completed(
        &self,
        action: ExtensionHostAction,
        args: serde_json::Map<String, Value>,
    ) -> Result<Value, String> {
        match self.dispatch_with_outcome(action, args)? {
            ExtensionHostActionOutcome::Completed(value) => Ok(value),
            ExtensionHostActionOutcome::Pending(request) => Err(format!(
                "host action '{}' was queued as {:?}; use the outcome-returning method at a mode boundary",
                action.protocol_name(),
                request
            )),
        }
    }

    fn array_result(
        &self,
        action: ExtensionHostAction,
        args: serde_json::Map<String, Value>,
    ) -> Result<Vec<Value>, String> {
        self.dispatch_completed(action, args)?
            .as_array()
            .cloned()
            .ok_or_else(|| {
                format!(
                    "host action '{}' returned a non-array result",
                    action.protocol_name()
                )
            })
    }

    /// The upstream `sessionManager` capability, represented by the live
    /// host adapter. It is a handle, not a copied manager object.
    pub fn session_manager(&self) -> Result<Self, String> {
        self.actions()?;
        Ok(self.clone())
    }

    /// The upstream `modelRegistry` capability, represented by the live host
    /// adapter. Model/catalog methods are available on the returned handle.
    pub fn model_registry(&self) -> Result<Self, String> {
        self.actions()?;
        Ok(self.clone())
    }

    pub fn snapshot(&self) -> Result<Value, String> {
        Ok(self.actions()?.snapshot())
    }

    pub fn snapshot_for(&self, request: &Value) -> Result<Value, String> {
        Ok(self.actions()?.snapshot_for(request))
    }

    pub fn wait_for_idle(&self) -> Result<(), String> {
        self.require_blocking(ExtensionHostAction::WaitForIdle)?;
        match self.dispatch_with_outcome(ExtensionHostAction::WaitForIdle, Default::default())? {
            ExtensionHostActionOutcome::Completed(_) => Ok(()),
            ExtensionHostActionOutcome::Pending(request) => Err(format!(
                "waitForIdle unexpectedly remained pending: {request:?}"
            )),
        }
    }

    pub fn new_session(
        &self,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::NewSession,
            serde_json::Map::from_iter([("options".to_string(), options.unwrap_or(Value::Null))]),
        )
    }

    pub fn fork(
        &self,
        entry_id: Option<&str>,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::Fork,
            serde_json::Map::from_iter([
                (
                    "entryId".to_string(),
                    entry_id
                        .map(|value| Value::String(value.to_string()))
                        .unwrap_or(Value::Null),
                ),
                ("options".to_string(), options.unwrap_or(Value::Null)),
            ]),
        )
    }

    pub fn navigate_tree(
        &self,
        target_id: Option<&str>,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::NavigateTree,
            serde_json::Map::from_iter([
                (
                    "targetId".to_string(),
                    target_id
                        .map(|value| Value::String(value.to_string()))
                        .unwrap_or(Value::Null),
                ),
                ("options".to_string(), options.unwrap_or(Value::Null)),
            ]),
        )
    }

    pub fn switch_session(
        &self,
        session_path: Option<&str>,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::SwitchSession,
            serde_json::Map::from_iter([
                (
                    "sessionPath".to_string(),
                    session_path
                        .map(|value| Value::String(value.to_string()))
                        .unwrap_or(Value::Null),
                ),
                ("options".to_string(), options.unwrap_or(Value::Null)),
            ]),
        )
    }

    pub fn reload(&self, options: Option<Value>) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::Reload,
            serde_json::Map::from_iter([("options".to_string(), options.unwrap_or(Value::Null))]),
        )
    }

    pub fn send_message(
        &self,
        message: Value,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::SendMessage,
            serde_json::Map::from_iter([
                ("message".to_string(), message),
                ("options".to_string(), options.unwrap_or(Value::Null)),
            ]),
        )
    }

    pub fn send_user_message(
        &self,
        content: Value,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::SendUserMessage,
            serde_json::Map::from_iter([
                ("content".to_string(), content),
                ("options".to_string(), options.unwrap_or(Value::Null)),
            ]),
        )
    }

    pub fn append_entry(
        &self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::AppendEntry,
            serde_json::Map::from_iter([
                (
                    "customType".to_string(),
                    Value::String(custom_type.to_string()),
                ),
                ("data".to_string(), data.unwrap_or(Value::Null)),
            ]),
        )
    }

    pub fn set_session_name(&self, name: Option<&str>) -> Result<(), String> {
        self.dispatch_completed(
            ExtensionHostAction::SetSessionName,
            serde_json::Map::from_iter([(
                "name".to_string(),
                name.map(|value| Value::String(value.to_string()))
                    .unwrap_or(Value::Null),
            )]),
        )
        .map(|_| ())
    }

    pub fn get_session_name(&self) -> Result<Option<String>, String> {
        match self.dispatch_completed(ExtensionHostAction::GetSessionName, Default::default())? {
            Value::Null => Ok(None),
            Value::String(value) => Ok(Some(value)),
            value => Err(format!("getSessionName returned unexpected value: {value}")),
        }
    }

    /// Set or clear the label on one session entry. The host preserves the
    /// request so mode-specific label application remains non-reentrant.
    pub fn set_label(&self, entry_id: &str, label: Option<&str>) -> Result<(), String> {
        let args = serde_json::Map::from_iter([
            ("entryId".to_string(), Value::String(entry_id.to_string())),
            (
                "label".to_string(),
                label
                    .map(|value| Value::String(value.to_string()))
                    .unwrap_or(Value::Null),
            ),
        ]);
        self.dispatch_completed(ExtensionHostAction::SetLabel, args)
            .map(|_| ())
    }

    /// Dispatch raw upstream label arguments for a mode adapter that already
    /// has a protocol object.
    pub fn set_label_args(&self, args: Value) -> Result<(), String> {
        let args = args.as_object().cloned().ok_or_else(|| {
            "setLabel expects an object containing the upstream label arguments".to_string()
        })?;
        self.dispatch_completed(ExtensionHostAction::SetLabel, args)
            .map(|_| ())
    }

    pub fn get_active_tools(&self) -> Result<Vec<String>, String> {
        self.array_result(ExtensionHostAction::GetActiveTools, Default::default())?
            .into_iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| "getActiveTools returned a non-string tool name".to_string())
            })
            .collect()
    }

    pub fn get_all_tools(&self) -> Result<Vec<Value>, String> {
        self.array_result(ExtensionHostAction::GetAllTools, Default::default())
    }

    pub fn set_active_tools(&self, tool_names: &[String]) -> Result<(), String> {
        self.dispatch_completed(
            ExtensionHostAction::SetActiveTools,
            serde_json::Map::from_iter([(
                "toolNames".to_string(),
                Value::Array(tool_names.iter().cloned().map(Value::String).collect()),
            )]),
        )
        .map(|_| ())
    }

    pub fn get_commands(&self) -> Result<Vec<Value>, String> {
        self.array_result(ExtensionHostAction::GetCommands, Default::default())
    }

    pub fn set_model(&self, model: Value) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::SetModel,
            serde_json::Map::from_iter([("model".to_string(), model)]),
        )
    }

    pub fn get_thinking_level(&self) -> Result<String, String> {
        match self.dispatch_completed(ExtensionHostAction::GetThinkingLevel, Default::default())? {
            Value::String(value) => Ok(value),
            value => Err(format!(
                "getThinkingLevel returned unexpected value: {value}"
            )),
        }
    }

    pub fn set_thinking_level(&self, level: &str) -> Result<(), String> {
        self.dispatch_completed(
            ExtensionHostAction::SetThinkingLevel,
            serde_json::Map::from_iter([("level".to_string(), Value::String(level.to_string()))]),
        )
        .map(|_| ())
    }

    pub fn model(&self) -> Result<Option<Value>, String> {
        match self.dispatch_completed(ExtensionHostAction::GetModel, Default::default())? {
            Value::Null => Ok(None),
            value => Ok(Some(value)),
        }
    }

    pub fn get_model(&self) -> Result<Option<Value>, String> {
        self.model()
    }

    pub fn scoped_models(&self) -> Result<Vec<Value>, String> {
        self.array_result(ExtensionHostAction::GetScopedModels, Default::default())
    }

    pub fn get_scoped_models(&self) -> Result<Vec<Value>, String> {
        self.scoped_models()
    }

    pub fn is_idle(&self) -> Result<bool, String> {
        match self.dispatch_completed(ExtensionHostAction::IsIdle, Default::default())? {
            Value::Bool(value) => Ok(value),
            value => Err(format!("isIdle returned unexpected value: {value}")),
        }
    }

    pub fn is_project_trusted(&self) -> Result<bool, String> {
        match self.dispatch_completed(ExtensionHostAction::IsProjectTrusted, Default::default())? {
            Value::Bool(value) => Ok(value),
            value => Err(format!(
                "isProjectTrusted returned unexpected value: {value}"
            )),
        }
    }

    pub fn signal(&self) -> Result<Option<Value>, String> {
        match self.dispatch_with_outcome(ExtensionHostAction::GetSignal, Default::default())? {
            ExtensionHostActionOutcome::Completed(Value::Null) => Ok(None),
            ExtensionHostActionOutcome::Completed(value) => Ok(Some(value)),
            ExtensionHostActionOutcome::Pending(request) => Err(format!(
                "getSignal unexpectedly remained pending: {request:?}"
            )),
        }
    }

    pub fn get_signal(&self) -> Result<Option<Value>, String> {
        self.signal()
    }

    pub fn abort(&self, tool_call_id: Option<&str>) -> Result<ExtensionHostActionOutcome, String> {
        let mut args = serde_json::Map::new();
        if let Some(tool_call_id) = tool_call_id {
            args.insert(
                "toolCallId".to_string(),
                Value::String(tool_call_id.to_string()),
            );
        }
        self.dispatch_with_outcome(ExtensionHostAction::Abort, args)
    }

    pub fn has_pending_messages(&self) -> Result<bool, String> {
        match self
            .dispatch_completed(ExtensionHostAction::HasPendingMessages, Default::default())?
        {
            Value::Bool(value) => Ok(value),
            value => Err(format!(
                "hasPendingMessages returned unexpected value: {value}"
            )),
        }
    }

    pub fn shutdown(
        &self,
        tool_call_id: Option<&str>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        let mut args = serde_json::Map::new();
        if let Some(tool_call_id) = tool_call_id {
            args.insert(
                "toolCallId".to_string(),
                Value::String(tool_call_id.to_string()),
            );
        }
        self.dispatch_with_outcome(ExtensionHostAction::Shutdown, args)
    }

    pub fn get_context_usage(&self) -> Result<Option<Value>, String> {
        match self.dispatch_completed(ExtensionHostAction::GetContextUsage, Default::default())? {
            Value::Null => Ok(None),
            value => Ok(Some(value)),
        }
    }

    pub fn compact(&self, options: Option<Value>) -> Result<ExtensionHostActionOutcome, String> {
        self.dispatch_with_outcome(
            ExtensionHostAction::Compact,
            serde_json::Map::from_iter([("options".to_string(), options.unwrap_or(Value::Null))]),
        )
    }

    pub fn system_prompt(&self) -> Result<String, String> {
        match self.dispatch_completed(ExtensionHostAction::GetSystemPrompt, Default::default())? {
            Value::String(value) => Ok(value),
            value => Err(format!("systemPrompt returned unexpected value: {value}")),
        }
    }

    pub fn system_prompt_options(&self) -> Result<Value, String> {
        self.dispatch_completed(
            ExtensionHostAction::GetSystemPromptOptions,
            Default::default(),
        )
    }

    pub fn tool_update(&self, tool_call_id: Option<&str>, result: Value) -> Result<(), String> {
        let mut args = serde_json::Map::from_iter([("result".to_string(), result)]);
        if let Some(tool_call_id) = tool_call_id {
            args.insert(
                "toolCallId".to_string(),
                Value::String(tool_call_id.to_string()),
            );
        }
        self.dispatch_completed(ExtensionHostAction::ToolUpdate, args)
            .map(|_| ())
    }
}

/// Per-tool execution mode override from upstream `ToolExecutionMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

impl ToolExecutionMode {
    pub const fn protocol_name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
        }
    }
}

/// Controls whether the host draws the standard tool shell around a render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolRenderShell {
    #[default]
    Default,
    SelfRendered,
}

impl ToolRenderShell {
    pub const fn protocol_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::SelfRendered => "self",
        }
    }
}

/// Native JSON form of upstream `prepareArguments`.
///
/// The callback runs before schema validation. Returning the input unchanged
/// is the Rust equivalent of leaving the upstream compatibility shim absent.
pub type ToolPrepareArgumentsFn = Arc<dyn Fn(Value) -> Value + Send + Sync + 'static>;

/// Open JSON form of an upstream `onUpdate` callback.
///
/// The value is an `AgentToolResult`-shaped object. The adapter validates and
/// forwards it to the host's live tool-update callback before execution
/// continues.
pub type ToolUpdateFn = Arc<dyn Fn(Value) -> Result<(), String> + Send + Sync + 'static>;

/// Opaque render context passed to a native `renderCall` callback.
///
/// The structured fields mirror upstream `ToolRenderContext`; `args`,
/// `last_component`, and `state` remain JSON so a native renderer can carry
/// arbitrary extension-owned values without coupling this contract to
/// `pi-tui` components.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRenderContext {
    pub args: Value,
    pub tool_call_id: String,
    pub last_component: Option<Value>,
    pub state: Value,
    pub cwd: String,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
}

/// JSON request passed to a registered `renderCall` callback.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRenderCallRequest {
    pub args: Value,
    pub theme: Value,
    pub context: ToolRenderContext,
}

/// The known upstream result-render options. Extra options can remain in the
/// surrounding JSON request when a future host adds fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRenderResultOptions {
    pub expanded: bool,
    pub is_partial: bool,
}

/// JSON request passed to a registered `renderResult` callback.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRenderResultRequest {
    pub result: Value,
    pub options: ToolRenderResultOptions,
    pub theme: Value,
    pub context: ToolRenderContext,
}

/// Native open-JSON form of upstream `renderCall`.
pub type ToolRenderCallFn =
    Arc<dyn Fn(ToolRenderCallRequest) -> Result<Value, String> + Send + Sync + 'static>;

/// Native open-JSON form of upstream `renderResult`.
pub type ToolRenderResultFn =
    Arc<dyn Fn(ToolRenderResultRequest) -> Result<Value, String> + Send + Sync + 'static>;

/// JSON request passed to a registered tool execute closure.
///
/// The required native tool contract is `tool_call_id`, prepared `params`, a
/// live [`ExtensionContext`], and an optional live `on_update` callback. The
/// current abort signal is available through `context.signal()`/`context.host.signal()`;
/// callers can use [`Self::update`] for the exact upstream `onUpdate` behavior.
#[derive(Clone)]
pub struct ToolExecutionRequest {
    pub tool_call_id: String,
    pub params: Value,
    pub context: ExtensionContext,
    pub on_update: Option<ToolUpdateFn>,
}

impl std::fmt::Debug for ToolExecutionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolExecutionRequest")
            .field("tool_call_id", &self.tool_call_id)
            .field("params", &self.params)
            .field("context", &self.context)
            .field("has_on_update", &self.on_update.is_some())
            .finish()
    }
}

impl ToolExecutionRequest {
    /// Emit a live partial result through the upstream-shaped update callback.
    /// When no agent callback was supplied, the update is retained by the
    /// bound host context so direct runner callers observe the same action.
    pub fn update(&self, result: Value) -> Result<(), String> {
        if let Some(on_update) = &self.on_update {
            on_update(result)
        } else {
            self.context
                .host
                .tool_update(Some(&self.tool_call_id), result)
        }
    }
}

/// A live tool callback. The JSON result is intentionally open-shaped to
/// preserve the upstream AgentToolResult boundary without coupling this
/// extension crate to one renderer or agent-loop representation.
pub type ToolExecuteFn =
    Arc<dyn Fn(ToolExecutionRequest) -> Result<Value, String> + Send + Sync + 'static>;

/// JSON-only bridge callback for a native provider stream function.
///
/// The model, context, and options remain opaque at this layer so the
/// bridge can carry the upstream provider event protocol without coupling
/// extension loading to `pi-ai` types.
pub type NativeProviderCallbackFn =
    Arc<dyn Fn(Value, Value, Value) -> Result<Vec<Value>, String> + Send + Sync + 'static>;

/// A registered extension tool (upstream `ToolDefinition`/`RegisteredTool`).
///
/// All upstream definition fields are retained in Rust-native form. Provider
/// sampling remains open JSON (`false` or a constrained-sampling object), and
/// renderer callbacks return open JSON component descriptors so the extension
/// layer does not depend on a particular terminal renderer. The live execute
/// callback receives prepared arguments, the bound context, and an optional
/// update callback.
#[derive(Clone, Default)]
pub struct RegisteredTool {
    pub name: String,
    /// Human-readable label used by the host tool row.
    pub label: String,
    pub description: String,
    /// Optional one-line contribution to the available-tools prompt section.
    pub prompt_snippet: Option<String>,
    /// Optional guideline bullets for the active system prompt.
    pub prompt_guidelines: Option<Vec<String>>,
    /// Parameter schema (JSON Schema-ish value; upstream uses TypeBox).
    pub parameters: Value,
    /// Provider-side constrained sampling (`false` or an open config object).
    pub constrained_sampling: Option<Value>,
    /// Whether the host draws the standard shell or the renderer owns framing.
    pub render_shell: ToolRenderShell,
    /// Raw argument preparation before schema validation.
    pub prepare_arguments: Option<ToolPrepareArgumentsFn>,
    /// Per-tool sequential/parallel execution override.
    pub execution_mode: Option<ToolExecutionMode>,
    pub source_info: SourceInfo,
    /// Live execute closure when the extension runtime can provide one.
    /// Metadata-only registrations leave this as `None`.
    pub execute: Option<ToolExecuteFn>,
    /// Optional open-JSON call renderer.
    pub render_call: Option<ToolRenderCallFn>,
    /// Optional open-JSON result renderer.
    pub render_result: Option<ToolRenderResultFn>,
}

impl std::fmt::Debug for RegisteredTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredTool")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("description", &self.description)
            .field("prompt_snippet", &self.prompt_snippet)
            .field("prompt_guidelines", &self.prompt_guidelines)
            .field("parameters", &self.parameters)
            .field("constrained_sampling", &self.constrained_sampling)
            .field("render_shell", &self.render_shell)
            .field("has_prepare_arguments", &self.prepare_arguments.is_some())
            .field("execution_mode", &self.execution_mode)
            .field("source_info", &self.source_info)
            .field("has_execute", &self.execute.is_some())
            .field("has_render_call", &self.render_call.is_some())
            .field("has_render_result", &self.render_result.is_some())
            .finish()
    }
}

/// A registered CLI flag (upstream `ExtensionFlag`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionFlag {
    pub name: String,
    pub description: Option<String>,
    pub flag_type: FlagType,
    pub default: Option<Value>,
    pub extension_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagType {
    Boolean,
    String,
}

/// A registered keyboard shortcut (upstream `ExtensionShortcut`).
#[derive(Clone)]
pub struct ExtensionShortcut {
    pub shortcut: String,
    pub description: Option<String>,
    pub handler: HandlerFn,
    pub extension_path: String,
}

/// A registered command (upstream `RegisteredCommand`).
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub source_info: SourceInfo,
    pub description: Option<String>,
    pub handler: HandlerFn,
}

/// A command with its invocation name resolved (upstream `ResolvedCommand`).
#[derive(Clone)]
pub struct ResolvedCommand {
    pub name: String,
    pub invocation_name: String,
    pub source_info: SourceInfo,
    pub description: Option<String>,
    pub handler: HandlerFn,
}

/// A loaded extension with all registered items (upstream `Extension`).
#[derive(Clone, Default)]
pub struct Extension {
    pub path: String,
    pub resolved_path: String,
    /// Keeps the mode-scoped runtime alive for external callback closures.
    /// The bridge process holds only a weak reference so invalidation can
    /// terminate it without creating an `Extension -> process -> runtime`
    /// ownership cycle.
    pub runtime: Option<Arc<Mutex<ExtensionRuntime>>>,
    pub hidden: bool,
    pub source_info: SourceInfo,
    pub handlers: BTreeMap<String, Vec<HandlerFn>>,
    pub tools: BTreeMap<String, RegisteredTool>,
    pub message_renderers: BTreeMap<String, MessageRenderer>,
    pub entry_renderers: BTreeMap<String, EntryRenderer>,
    pub markdown_transformer: Option<MarkdownTransformer>,
    pub registrations: Vec<RegistrationRecord>,
    pub commands: BTreeMap<String, RegisteredCommand>,
    pub flags: BTreeMap<String, ExtensionFlag>,
    pub shortcuts: BTreeMap<String, ExtensionShortcut>,
}

impl std::fmt::Debug for Extension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Handlers are opaque closures; summarize the rest.
        f.debug_struct("Extension")
            .field("path", &self.path)
            .field("resolved_path", &self.resolved_path)
            .field("hidden", &self.hidden)
            .field("source_info", &self.source_info)
            .field("handler_events", &self.handlers.keys().collect::<Vec<_>>())
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("commands", &self.commands.keys().collect::<Vec<_>>())
            .field("flags", &self.flags.keys().collect::<Vec<_>>())
            .field("shortcuts", &self.shortcuts.keys().collect::<Vec<_>>())
            .field(
                "message_renderers",
                &self.message_renderers.keys().collect::<Vec<_>>(),
            )
            .field(
                "entry_renderers",
                &self.entry_renderers.keys().collect::<Vec<_>>(),
            )
            .field(
                "has_markdown_transformer",
                &self.markdown_transformer.is_some(),
            )
            .field("registrations", &self.registrations)
            .finish()
    }
}

impl Extension {
    pub fn record_registration(&mut self, kind: RegistrationKind, name: Option<String>) {
        self.registrations.push(RegistrationRecord { kind, name });
    }

    fn ordered_names(
        &self,
        kind: RegistrationKind,
        names: impl Iterator<Item = String>,
    ) -> Vec<String> {
        let mut ordered = Vec::new();
        let mut seen = BTreeSet::new();
        for registration in &self.registrations {
            if registration.kind == kind {
                if let Some(name) = &registration.name {
                    if seen.insert(name.clone()) {
                        ordered.push(name.clone());
                    }
                }
            }
        }
        for name in names {
            if seen.insert(name.clone()) {
                ordered.push(name);
            }
        }
        ordered
    }

    pub fn ordered_handler_events(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Handler, self.handlers.keys().cloned())
    }

    pub fn ordered_tool_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Tool, self.tools.keys().cloned())
    }

    pub fn ordered_command_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Command, self.commands.keys().cloned())
    }

    pub fn ordered_flag_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Flag, self.flags.keys().cloned())
    }

    pub fn ordered_shortcut_names(&self) -> Vec<String> {
        self.ordered_names(RegistrationKind::Shortcut, self.shortcuts.keys().cloned())
    }

    pub fn ordered_message_renderer_names(&self) -> Vec<String> {
        self.ordered_names(
            RegistrationKind::MessageRenderer,
            self.message_renderers.keys().cloned(),
        )
    }

    pub fn ordered_entry_renderer_names(&self) -> Vec<String> {
        self.ordered_names(
            RegistrationKind::EntryRenderer,
            self.entry_renderers.keys().cloned(),
        )
    }
}

/// Result of loading extensions (upstream `LoadExtensionsResult`).
#[derive(Clone, Default)]
pub struct LoadExtensionsResult {
    pub extensions: Vec<Extension>,
    pub errors: Vec<ExtensionLoadError>,
    pub runtime: Arc<Mutex<ExtensionRuntime>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionLoadError {
    pub path: String,
    pub error: String,
}

/// Extension error emitted at runtime (upstream `ExtensionError`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
}
/// Extension context shared by native handlers (upstream `ExtensionContext`).
///
/// `host` is the live Rust capability handle for the upstream session manager,
/// model registry, state properties, and host actions. It is intentionally
/// separate from the JSON event payload so a handler can call the host while
/// the mode retains ownership of its agent/session objects.
#[derive(Debug, Clone, Default)]
pub struct ExtensionContext {
    pub mode: String,
    pub cwd: String,
    pub has_ui: bool,
    pub ui: ExtensionUiContext,
    pub host: ExtensionHostContext,
}

impl ExtensionContext {
    /// Return the host capability view corresponding to the upstream
    /// `sessionManager` property.
    pub fn session_manager(&self) -> Result<ExtensionHostContext, String> {
        self.host.session_manager()
    }

    /// Return the host capability view corresponding to the upstream
    /// `modelRegistry` property.
    pub fn model_registry(&self) -> Result<ExtensionHostContext, String> {
        self.host.model_registry()
    }

    pub fn model(&self) -> Result<Option<Value>, String> {
        self.host.model()
    }

    pub fn scoped_models(&self) -> Result<Vec<Value>, String> {
        self.host.scoped_models()
    }

    pub fn thinking_level(&self) -> Result<String, String> {
        self.host.get_thinking_level()
    }

    pub fn is_idle(&self) -> Result<bool, String> {
        self.host.is_idle()
    }

    pub fn is_project_trusted(&self) -> Result<bool, String> {
        self.host.is_project_trusted()
    }

    pub fn signal(&self) -> Result<Option<Value>, String> {
        self.host.signal()
    }

    pub fn abort(&self) -> Result<ExtensionHostActionOutcome, String> {
        self.host.abort(None)
    }

    pub fn has_pending_messages(&self) -> Result<bool, String> {
        self.host.has_pending_messages()
    }

    pub fn shutdown(&self) -> Result<ExtensionHostActionOutcome, String> {
        self.host.shutdown(None)
    }

    pub fn send_message(
        &self,
        message: Value,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.send_message(message, options)
    }

    pub fn send_user_message(
        &self,
        content: Value,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.send_user_message(content, options)
    }

    pub fn append_entry(
        &self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.append_entry(custom_type, data)
    }

    pub fn set_session_name(&self, name: Option<&str>) -> Result<(), String> {
        self.host.set_session_name(name)
    }

    pub fn get_session_name(&self) -> Result<Option<String>, String> {
        self.host.get_session_name()
    }

    pub fn set_label(&self, entry_id: &str, label: Option<&str>) -> Result<(), String> {
        self.host.set_label(entry_id, label)
    }

    pub fn set_label_args(&self, args: Value) -> Result<(), String> {
        self.host.set_label_args(args)
    }

    pub fn get_active_tools(&self) -> Result<Vec<String>, String> {
        self.host.get_active_tools()
    }

    pub fn get_all_tools(&self) -> Result<Vec<Value>, String> {
        self.host.get_all_tools()
    }

    pub fn set_active_tools(&self, tool_names: &[String]) -> Result<(), String> {
        self.host.set_active_tools(tool_names)
    }

    pub fn get_commands(&self) -> Result<Vec<Value>, String> {
        self.host.get_commands()
    }

    pub fn set_model(&self, model: Value) -> Result<ExtensionHostActionOutcome, String> {
        self.host.set_model(model)
    }

    pub fn get_thinking_level(&self) -> Result<String, String> {
        self.host.get_thinking_level()
    }

    pub fn set_thinking_level(&self, level: &str) -> Result<(), String> {
        self.host.set_thinking_level(level)
    }

    pub fn get_model(&self) -> Result<Option<Value>, String> {
        self.host.get_model()
    }

    pub fn get_scoped_models(&self) -> Result<Vec<Value>, String> {
        self.host.get_scoped_models()
    }

    pub fn get_context_usage(&self) -> Result<Option<Value>, String> {
        self.host.get_context_usage()
    }

    pub fn compact(&self, options: Option<Value>) -> Result<ExtensionHostActionOutcome, String> {
        self.host.compact(options)
    }

    pub fn get_system_prompt(&self) -> Result<String, String> {
        self.host.system_prompt()
    }

    pub fn get_system_prompt_options(&self) -> Result<Value, String> {
        self.host.system_prompt_options()
    }

    pub fn tool_update(&self, result: Value) -> Result<(), String> {
        self.host.tool_update(None, result)
    }

    /// The command-context-only upstream methods are exposed here with
    /// explicit Rust outcomes. `Pending` is retained for mode-owned work.
    pub fn wait_for_idle(&self) -> Result<(), String> {
        self.host.wait_for_idle()
    }

    pub fn new_session(
        &self,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.new_session(options)
    }

    pub fn fork(
        &self,
        entry_id: Option<&str>,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.fork(entry_id, options)
    }

    pub fn navigate_tree(
        &self,
        target_id: Option<&str>,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.navigate_tree(target_id, options)
    }

    pub fn switch_session(
        &self,
        session_path: Option<&str>,
        options: Option<Value>,
    ) -> Result<ExtensionHostActionOutcome, String> {
        self.host.switch_session(session_path, options)
    }

    pub fn reload(&self, options: Option<Value>) -> Result<ExtensionHostActionOutcome, String> {
        self.host.reload(options)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    Continue,
    Transform,
    Handled,
    Consume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEventResult {
    pub action: InputAction,
    pub text: Option<String>,
    pub images: Option<Value>,
}

impl InputEventResult {
    pub fn continue_with(text: impl Into<String>, images: Option<Value>) -> Self {
        Self {
            action: InputAction::Continue,
            text: Some(text.into()),
            images,
        }
    }

    pub fn transform(text: impl Into<String>, images: Option<Value>) -> Self {
        Self {
            action: InputAction::Transform,
            text: Some(text.into()),
            images,
        }
    }

    pub fn consume() -> Self {
        Self {
            action: InputAction::Handled,
            text: None,
            images: None,
        }
    }

    pub fn handled() -> Self {
        Self::consume()
    }
}

/// Shared runtime state created by the loader (upstream
/// `ExtensionRuntimeState` + `ExtensionRuntime`). A loaded runtime becomes
/// active when its host actions and correlated UI broker are bound; registration
/// state remains available while the loader is constructing the runtime.
#[derive(Default)]
pub struct ExtensionRuntime {
    pub flag_values: BTreeMap<String, Value>,
    pub pending_provider_registrations: Vec<PendingProviderRegistration>,
    pub pending_native_provider_registrations: Vec<PendingNativeProviderRegistration>,
    initialized: bool,
    host_actions: Option<Arc<dyn ExtensionHostActions>>,
    ui_broker: ExtensionUiBroker,
    active: Arc<AtomicBool>,
    stale_message: Option<String>,
    subscriptions: Arc<Mutex<Vec<Subscription>>>,
}

impl std::fmt::Debug for ExtensionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRuntime")
            .field("flag_values", &self.flag_values)
            .field(
                "pending_provider_registrations",
                &self.pending_provider_registrations,
            )
            .field(
                "pending_native_provider_registrations",
                &self.pending_native_provider_registrations,
            )
            .field("initialized", &self.initialized)
            .field("has_host_actions", &self.host_actions.is_some())
            .field("ui_broker", &self.ui_broker)
            .field("stale_message", &self.stale_message)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingProviderRegistration {
    pub name: String,
    pub config: Value,
    pub extension_path: String,
}

#[derive(Clone)]
pub struct PendingNativeProviderRegistration {
    pub provider: String,
    /// Provider object fields excluding executable callback functions.
    pub definition: Value,
    pub callbacks: BTreeMap<String, NativeProviderCallbackFn>,
    pub extension_path: String,
}

impl std::fmt::Debug for PendingNativeProviderRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingNativeProviderRegistration")
            .field("provider", &self.provider)
            .field("definition", &self.definition)
            .field("callbacks", &self.callbacks.keys().collect::<Vec<_>>())
            .field("extension_path", &self.extension_path)
            .finish()
    }
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark this extension instance stale (upstream `invalidate`). All
    /// further action/state access throws `assertActive` errors.
    pub fn invalidate(&mut self, message: Option<&str>) {
        if self.stale_message.is_none() {
            self.stale_message = Some(
                message
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| STALE_MESSAGE.to_string()),
            );
            self.active.store(false, Ordering::Release);
            // Drop queued registrations on invalidation (upstream unsubscribes
            // event-bus handlers; the port clears queued provider work).
            self.pending_provider_registrations.clear();
            self.pending_native_provider_registrations.clear();
            self.ui_broker.cancel_all();
            self.ui_broker.clear_terminal_input_listeners();
            let subscriptions = self
                .subscriptions
                .lock()
                .map(|mut callbacks| std::mem::take(&mut *callbacks))
                .unwrap_or_default();
            for unsubscribe in subscriptions {
                let _ = catch_unwind(AssertUnwindSafe(|| unsubscribe()));
            }
        }
    }

    pub fn is_stale(&self) -> bool {
        self.stale_message.is_some()
    }

    /// Reject runtime actions after a session replacement or reload.
    pub fn assert_active(&self) -> Result<(), String> {
        match &self.stale_message {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }

    pub fn assert_initialized(&self) -> Result<(), String> {
        if self.initialized {
            Ok(())
        } else {
            Err(NOT_INITIALIZED_MESSAGE.to_string())
        }
    }

    pub fn stale_error(&self) -> Option<String> {
        self.stale_message.clone()
    }

    /// Track an event-bus subscription so session invalidation tears it down.
    /// The returned cleanup is idempotent from the caller's perspective; the
    /// runtime owns the authoritative invalidation cleanup.
    pub fn track_event_bus_subscription(
        &self,
        unsubscribe: Arc<dyn Fn() + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let called = Arc::clone(&called);
            move || {
                if !called.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    let _ = catch_unwind(AssertUnwindSafe(|| unsubscribe()));
                }
            }
        });
        if self.assert_active().is_ok() {
            if let Ok(mut callbacks) = self.subscriptions.lock() {
                callbacks.push(Arc::clone(&callback));
            }
        } else {
            callback();
        }
        Box::new(move || callback())
    }

    /// Whether the runner bound concrete actions (upstream `bindCore`).
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn bind_core(&mut self) {
        self.initialized = true;
        if self.stale_message.is_none() {
            self.active.store(true, Ordering::Release);
        }
    }

    /// Bind the host-owned action callbacks used by live external extension
    /// callbacks. This is separate from `bind_core()` so existing Rust-native
    /// callers can still mark the runtime initialized without inventing host
    /// behavior.
    pub fn bind_core_with_actions(&mut self, actions: Arc<dyn ExtensionHostActions>) {
        self.ui_broker = actions.ui_broker();
        self.host_actions = Some(actions);
        self.initialized = true;
        if self.stale_message.is_none() {
            self.active.store(true, Ordering::Release);
        }
    }

    /// Build the context exposed to a native callback. Dialog methods are
    /// enabled only when the mode has UI and the caller has proven that its
    /// synchronous callback is executing off the input loop.
    pub fn ui_context(&self, has_ui: bool, blocking_allowed: bool) -> ExtensionUiContext {
        ExtensionUiContext::new_with_active(
            self.ui_broker.clone(),
            has_ui,
            blocking_allowed,
            Some(self.active.clone()),
        )
    }

    /// Build the live host capability handle for a callback. An unbound
    /// runtime returns a handle whose methods fail explicitly, which keeps
    /// native context construction infallible while preventing silent no-op
    /// host calls.
    pub fn host_context(&self, blocking_allowed: bool) -> ExtensionHostContext {
        self.host_context_for(blocking_allowed, None)
    }

    /// Build a host handle scoped to one tool call. Signal, abort, and tool
    /// update methods automatically carry this id to the host dispatcher.
    pub fn host_context_for(
        &self,
        blocking_allowed: bool,
        tool_call_id: Option<&str>,
    ) -> ExtensionHostContext {
        ExtensionHostContext::new(
            self.host_actions.clone(),
            blocking_allowed,
            tool_call_id,
            Some(self.active.clone()),
        )
    }

    /// Clone the bound host callback while the caller still owns the runtime
    /// guard, so it can be invoked after that guard is released.
    pub fn host_action_handler(&self) -> Result<Arc<dyn ExtensionHostActions>, String> {
        self.assert_active()?;
        self.host_actions
            .as_ref()
            .cloned()
            .ok_or_else(|| NOT_INITIALIZED_MESSAGE.to_string())
    }

    /// Snapshot host-owned synchronous getter state for an external callback.
    pub fn host_action_snapshot(&self) -> Result<Value, String> {
        self.assert_active()?;
        Ok(self
            .host_actions
            .as_ref()
            .map(|actions| actions.snapshot())
            .unwrap_or_else(|| Value::Object(Default::default())))
    }

    pub fn host_action_snapshot_for(&self, request: &Value) -> Result<Value, String> {
        self.assert_active()?;
        Ok(self
            .host_actions
            .as_ref()
            .map(|actions| actions.snapshot_for(request))
            .unwrap_or_else(|| Value::Object(Default::default())))
    }

    pub fn has_host_actions(&self) -> bool {
        self.host_actions.is_some()
    }

    /// Set a CLI flag value (upstream `setFlagValue`).
    pub fn set_flag_value(&mut self, name: &str, value: Value) {
        self.flag_values.insert(name.to_string(), value);
    }
}

impl LoadExtensionsResult {
    /// Bind host actions after loading the extension factories, preserving the
    /// shared runtime captured by every persistent external bridge.
    pub fn bind_core_with_actions(&self, actions: Arc<dyn ExtensionHostActions>) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.bind_core_with_actions(actions);
        }
    }
}

pub const STALE_MESSAGE: &str = "This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload().";
pub const NOT_INITIALIZED_MESSAGE: &str =
    "Extension runtime not initialized. Action methods cannot be called during extension loading.";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn runtime_flag_values_default_and_override() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .flag_values
            .insert("no-tools".to_string(), Value::Bool(true));
        assert_eq!(
            runtime.flag_values.get("no-tools"),
            Some(&Value::Bool(true))
        );
        runtime.set_flag_value("no-tools", Value::Bool(false));
        assert_eq!(
            runtime.flag_values.get("no-tools"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn invalidate_marks_stale_and_clears_queued() {
        let mut runtime = ExtensionRuntime::new();
        runtime
            .pending_provider_registrations
            .push(PendingProviderRegistration {
                name: "demo".into(),
                config: Value::Null,
                extension_path: "ext".into(),
            });
        runtime.invalidate(None);
        assert!(runtime.is_stale());
        assert!(runtime.pending_provider_registrations.is_empty());
    }

    #[test]
    fn invalidate_once_keeps_first_message() {
        let mut runtime = ExtensionRuntime::new();
        runtime.invalidate(Some("first"));
        runtime.invalidate(Some("second"));
        assert!(runtime.is_stale());
    }

    #[test]
    fn bind_core_enables_initialized() {
        let mut runtime = ExtensionRuntime::new();
        assert!(!runtime.is_initialized());
        runtime.bind_core();
        assert!(runtime.is_initialized());
    }

    #[test]
    fn context_host_action_names_are_typed() {
        for (name, expected) in [
            ("waitForIdle", ExtensionHostAction::WaitForIdle),
            ("newSession", ExtensionHostAction::NewSession),
            ("fork", ExtensionHostAction::Fork),
            ("navigateTree", ExtensionHostAction::NavigateTree),
            ("switchSession", ExtensionHostAction::SwitchSession),
            ("reload", ExtensionHostAction::Reload),
            ("getModel", ExtensionHostAction::GetModel),
            ("getScopedModels", ExtensionHostAction::GetScopedModels),
            ("isIdle", ExtensionHostAction::IsIdle),
            ("isProjectTrusted", ExtensionHostAction::IsProjectTrusted),
            ("getSignal", ExtensionHostAction::GetSignal),
            ("abort", ExtensionHostAction::Abort),
            (
                "hasPendingMessages",
                ExtensionHostAction::HasPendingMessages,
            ),
            ("shutdown", ExtensionHostAction::Shutdown),
            ("getContextUsage", ExtensionHostAction::GetContextUsage),
            ("compact", ExtensionHostAction::Compact),
            ("getSystemPrompt", ExtensionHostAction::GetSystemPrompt),
            (
                "getSystemPromptOptions",
                ExtensionHostAction::GetSystemPromptOptions,
            ),
            ("toolUpdate", ExtensionHostAction::ToolUpdate),
        ] {
            assert_eq!(
                ExtensionHostAction::from_protocol_name(name),
                Some(expected)
            );
        }
        assert_eq!(ExtensionHostAction::from_protocol_name("unknown"), None);
    }

    #[test]
    fn pending_host_action_preserves_bridge_continuation_metadata() {
        let request = PendingHostAction::new(
            ExtensionHostAction::SwitchSession,
            serde_json::json!({
                "sessionPath": "/tmp/session.jsonl",
                "options": {
                    "__bridgeContinuation": {"id": "switch-1"},
                },
            }),
            serde_json::json!({
                "type": "switch_session",
                "options": {
                    "__bridgeContinuation": {"id": "switch-1"},
                },
            }),
        );
        assert_eq!(request.action.protocol_name(), "switchSession");
        assert!(request.is_lifecycle());
        assert_eq!(
            request.continuation_metadata(),
            Some(&serde_json::json!({"id": "switch-1"}))
        );
        assert_eq!(
            request.args["options"]["__bridgeContinuation"],
            serde_json::json!({"id": "switch-1"})
        );
        assert_eq!(
            request.payload["options"]["__bridgeContinuation"],
            serde_json::json!({"id": "switch-1"})
        );
    }

    #[test]
    fn registered_tool_definition_metadata_is_preserved() {
        let prepare_arguments: ToolPrepareArgumentsFn = Arc::new(|arguments| arguments);
        let execute: ToolExecuteFn = Arc::new(|_| Ok(Value::Null));
        let render_call: ToolRenderCallFn = Arc::new(|_| Ok(Value::Null));
        let render_result: ToolRenderResultFn = Arc::new(|_| Ok(Value::Null));
        let tool = RegisteredTool {
            name: "metadata-tool".to_string(),
            label: "Metadata tool".to_string(),
            description: "retains every upstream definition field".to_string(),
            prompt_snippet: Some("Use metadata-tool when testing metadata.".to_string()),
            prompt_guidelines: Some(vec![
                "Keep the arguments small.".to_string(),
                "Report partial work.".to_string(),
            ]),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
            }),
            constrained_sampling: Some(serde_json::json!({
                "type": "json_schema",
                "strict": "prefer",
            })),
            render_shell: ToolRenderShell::SelfRendered,
            prepare_arguments: Some(prepare_arguments),
            execution_mode: Some(ToolExecutionMode::Parallel),
            source_info: SourceInfo::synthetic(
                "<inline:metadata-tool>",
                "rust-native",
                Some("/fixture".to_string()),
            ),
            execute: Some(execute),
            render_call: Some(render_call),
            render_result: Some(render_result),
        };

        assert_eq!(tool.name, "metadata-tool");
        assert_eq!(tool.label, "Metadata tool");
        assert_eq!(
            tool.prompt_snippet.as_deref(),
            Some("Use metadata-tool when testing metadata.")
        );
        assert_eq!(
            tool.prompt_guidelines.as_deref(),
            Some(
                [
                    "Keep the arguments small.".to_string(),
                    "Report partial work.".to_string(),
                ]
                .as_slice()
            )
        );
        assert_eq!(tool.parameters["properties"]["value"]["type"], "string");
        assert_eq!(
            tool.constrained_sampling,
            Some(serde_json::json!({
                "type": "json_schema",
                "strict": "prefer",
            }))
        );
        assert_eq!(tool.render_shell.protocol_name(), "self");
        assert_eq!(
            tool.execution_mode.map(ToolExecutionMode::protocol_name),
            Some("parallel")
        );
        assert_eq!(tool.source_info.source, "rust-native");
        assert!(tool.prepare_arguments.is_some());
        assert!(tool.execute.is_some());
        assert!(tool.render_call.is_some());
        assert!(tool.render_result.is_some());

        let debug = format!("{tool:?}");
        assert!(debug.contains("has_prepare_arguments: true"));
        assert!(debug.contains("has_execute: true"));
        assert!(debug.contains("has_render_call: true"));
        assert!(debug.contains("has_render_result: true"));
    }

    fn next_ui_request(broker: &ExtensionUiBroker) -> Value {
        for _ in 0..500 {
            if let Some(request) = broker.drain_requests().into_iter().next() {
                return request;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("extension UI request was not emitted");
    }

    fn request_id(request: &Value) -> String {
        request["id"]
            .as_str()
            .expect("extension UI request id")
            .to_string()
    }

    fn worker_ui_context(broker: &ExtensionUiBroker) -> ExtensionUiContext {
        ExtensionUiContext::new(broker.clone(), true, true)
    }

    #[test]
    fn extension_ui_broker_resolves_select_confirm_input_and_editor() {
        let broker = ExtensionUiBroker::new();

        let select_context = worker_ui_context(&broker);
        let select = thread::spawn(move || {
            select_context.select(
                "Choose a color",
                &["red".to_string(), "blue".to_string()],
                Some(Duration::from_secs(1)),
            )
        });
        let request = next_ui_request(&broker);
        assert_eq!(request["type"], "extension_ui_request");
        assert_eq!(request["method"], "select");
        assert_eq!(request["title"], "Choose a color");
        assert_eq!(request["options"], serde_json::json!(["red", "blue"]));
        assert_eq!(request["timeout"], 1_000);
        let id = request_id(&request);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "result": "blue"
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            select.join().expect("select worker"),
            Ok(Some("blue".to_string()))
        );

        let confirm_context = worker_ui_context(&broker);
        let confirm = thread::spawn(move || {
            confirm_context.confirm(
                "Continue?",
                "The operation is ready.",
                Some(Duration::from_secs(1)),
            )
        });
        let request = next_ui_request(&broker);
        assert_eq!(request["method"], "confirm");
        assert_eq!(request["message"], "The operation is ready.");
        let id = request_id(&request);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "confirmed": true
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(confirm.join().expect("confirm worker"), Ok(true));

        let input_context = worker_ui_context(&broker);
        let input = thread::spawn(move || {
            input_context.input("Name", Some("Your name"), Some(Duration::from_secs(1)))
        });
        let request = next_ui_request(&broker);
        assert_eq!(request["method"], "input");
        assert_eq!(request["placeholder"], "Your name");
        let id = request_id(&request);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "value": "Ada"
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            input.join().expect("input worker"),
            Ok(Some("Ada".to_string()))
        );

        let editor_context = worker_ui_context(&broker);
        let editor = thread::spawn(move || {
            editor_context.editor_with_options(
                "Edit prompt",
                Some("prefilled"),
                ExtensionUiDialogOptions {
                    timeout: Some(Duration::from_secs(1)),
                    cancel: None,
                },
            )
        });
        let request = next_ui_request(&broker);
        assert_eq!(request["method"], "editor");
        assert_eq!(request["prefill"], "prefilled");
        assert!(request.get("timeout").is_none());
        let id = request_id(&request);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "result": "edited text"
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            editor.join().expect("editor worker"),
            Ok(Some("edited text".to_string()))
        );
        assert!(broker.pending_ids().is_empty());
    }

    #[test]
    fn extension_ui_broker_handles_response_cancellation_timeout_and_late_response() {
        let broker = ExtensionUiBroker::new();

        let context = worker_ui_context(&broker);
        let cancelled = thread::spawn(move || {
            context.select_with_options(
                "Cancel me",
                &["one".to_string()],
                ExtensionUiDialogOptions {
                    timeout: Some(Duration::from_secs(1)),
                    cancel: None,
                },
            )
        });
        let request = next_ui_request(&broker);
        let id = request_id(&request);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "cancelled": true
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(cancelled.join().expect("cancelled worker"), Ok(None));

        let context = worker_ui_context(&broker);
        let timed_out =
            thread::spawn(move || context.input("Timeout", None, Some(Duration::from_millis(10))));
        let request = next_ui_request(&broker);
        let id = request_id(&request);
        assert_eq!(timed_out.join().expect("timeout worker"), Ok(None));
        assert!(broker.pending_ids().is_empty());
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "value": "too late"
            })),
            ExtensionUiResponseDisposition::LateResponse
        );
        assert!(broker
            .drain_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "late_response"
                && diagnostic.id.as_deref() == Some(id.as_str())));
    }

    #[test]
    fn extension_ui_broker_correlates_concurrent_dialog_ids() {
        let broker = ExtensionUiBroker::new();
        let (sender, receiver) = mpsc::channel();
        broker.set_request_sink(Arc::new(move |request| {
            sender
                .send(request)
                .map_err(|_| "UI request receiver closed".to_string())
        }));

        let left_context = worker_ui_context(&broker);
        let left =
            thread::spawn(move || left_context.input("left", None, Some(Duration::from_secs(1))));
        let right_context = worker_ui_context(&broker);
        let right =
            thread::spawn(move || right_context.input("right", None, Some(Duration::from_secs(1))));
        let first = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first concurrent request");
        let second = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("second concurrent request");
        assert_ne!(request_id(&first), request_id(&second));

        for request in [&second, &first] {
            let value = match request["title"].as_str() {
                Some("left") => "L",
                Some("right") => "R",
                other => panic!("unexpected concurrent title: {other:?}"),
            };
            assert_eq!(
                broker.handle_response(&serde_json::json!({
                    "type": "extension_ui_response",
                    "id": request_id(request),
                    "value": value
                })),
                ExtensionUiResponseDisposition::Resolved
            );
        }
        assert_eq!(left.join().expect("left worker"), Ok(Some("L".to_string())));
        assert_eq!(
            right.join().expect("right worker"),
            Ok(Some("R".to_string()))
        );
    }

    #[test]
    fn extension_ui_broker_reports_malformed_unknown_and_safe_blocking_errors() {
        let broker = ExtensionUiBroker::new();
        assert_eq!(
            broker.handle_response(&serde_json::json!({"type": "extension_ui_response"})),
            ExtensionUiResponseDisposition::Malformed
        );
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": "missing",
                "value": "not pending"
            })),
            ExtensionUiResponseDisposition::UnknownId
        );
        let diagnostics = broker.drain_diagnostics();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "malformed_response"));
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unknown_response_id"));

        let synchronous_context = ExtensionUiContext::new(broker.clone(), true, false);
        let error = synchronous_context
            .select(
                "Unsafe",
                &["option".to_string()],
                Some(Duration::from_secs(1)),
            )
            .expect_err("synchronous handler must not block on UI");
        assert_eq!(error, BLOCKING_UI_UNAVAILABLE_MESSAGE);
        assert!(broker.pending_ids().is_empty());

        let context = worker_ui_context(&broker);
        let pending =
            thread::spawn(move || context.input("Malformed", None, Some(Duration::from_secs(1))));
        let request = next_ui_request(&broker);
        let id = request_id(&request);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "value": 42
            })),
            ExtensionUiResponseDisposition::Malformed
        );
        assert_eq!(broker.pending_ids(), vec![id.clone()]);
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": id,
                "value": "valid"
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            pending.join().expect("malformed-response worker"),
            Ok(Some("valid".to_string()))
        );
    }

    #[test]
    fn extension_ui_broker_cancellation_signal_and_cancel_all_wake_waiters() {
        let broker = ExtensionUiBroker::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let context = worker_ui_context(&broker);
        let cancel_for_worker = cancel.clone();
        let signal_cancelled = thread::spawn(move || {
            context.input_with_options(
                "Signal cancel",
                None,
                ExtensionUiDialogOptions {
                    timeout: Some(Duration::from_secs(1)),
                    cancel: Some(cancel_for_worker),
                },
            )
        });
        let _request = next_ui_request(&broker);
        cancel.store(true, Ordering::Release);
        assert_eq!(
            signal_cancelled.join().expect("signal cancellation worker"),
            Ok(None)
        );

        let context = worker_ui_context(&broker);
        let shutdown_cancelled = thread::spawn(move || {
            context.editor_with_options("Shutdown", None, ExtensionUiDialogOptions::default())
        });
        let _request = next_ui_request(&broker);
        broker.cancel_all();
        assert_eq!(
            shutdown_cancelled
                .join()
                .expect("shutdown cancellation worker"),
            Ok(None)
        );
        assert!(broker.pending_ids().is_empty());
    }

    #[test]
    fn extension_ui_broker_emits_all_fire_and_forget_actions_and_tracks_editor_text() {
        let broker = ExtensionUiBroker::new();
        let context = worker_ui_context(&broker);
        let lines = vec!["line one".to_string(), "line two".to_string()];
        context.notify("hello", Some("warning")).expect("notify");
        context.set_status("branch", Some("main")).expect("status");
        context
            .set_working(Some("working"))
            .expect("working message");
        context.set_working_visible(true).expect("working visible");
        context
            .set_working_indicator(Some(serde_json::json!({"frames": ["*"]})))
            .expect("working indicator");
        context
            .set_widget("details", Some(&lines), Some("belowEditor"))
            .expect("widget");
        context.set_title("Pi Rust").expect("title");
        context.set_editor_text("draft").expect("set editor text");
        context.paste_to_editor("pasted").expect("paste");
        assert_eq!(context.get_editor_text(), "pasted");

        let requests = broker.drain_requests();
        assert_eq!(requests.len(), 9);
        let methods: Vec<_> = requests
            .iter()
            .filter_map(|request| request["method"].as_str())
            .collect();
        for method in [
            "notify",
            "setStatus",
            "setWorkingMessage",
            "setWorkingVisible",
            "setWorkingIndicator",
            "setWidget",
            "setTitle",
        ] {
            assert!(
                methods.contains(&method),
                "missing fire-and-forget method {method}"
            );
        }
        assert_eq!(
            methods
                .iter()
                .filter(|method| **method == "set_editor_text")
                .count(),
            2
        );
        assert_eq!(requests[0]["notifyType"], "warning");
        assert_eq!(requests[1]["statusKey"], "branch");
        assert_eq!(
            requests[5]["widgetLines"],
            serde_json::json!(["line one", "line two"])
        );
        assert_eq!(requests[5]["widgetPlacement"], "belowEditor");
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": request_id(&requests[0]),
                "value": "ignored"
            })),
            ExtensionUiResponseDisposition::FireAndForgetResponse
        );
        assert!(broker
            .drain_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "fire_and_forget_response"));
    }

    #[test]
    fn extension_status_is_retained_for_interactive_without_general_ui_delivery() {
        let broker = ExtensionUiBroker::disabled();
        let context = ExtensionUiContext::new(broker.clone(), true, false);

        context
            .set_status("build", Some("ready"))
            .expect("interactive status should not require the general UI sink");
        assert_eq!(
            broker.extension_statuses(),
            vec![("build".to_string(), "ready".to_string())]
        );
        assert!(broker.drain_requests().is_empty());

        context
            .set_status("build", None)
            .expect("status removal should be accepted");
        assert!(broker.extension_statuses().is_empty());
    }

    #[tokio::test]
    async fn extension_status_updates_wake_the_owner() {
        let broker = ExtensionUiBroker::disabled();
        let wake = broker.status_wakeup();
        let notified = wake.notified();
        broker
            .set_status("build", Some("ready"))
            .expect("status update");
        assert!(tokio::time::timeout(Duration::from_millis(20), notified)
            .await
            .is_ok());
    }

    #[test]
    fn extension_ui_broker_registers_dispatches_and_unsubscribes_terminal_input() {
        let broker = ExtensionUiBroker::new();
        let first_seen = Arc::new(Mutex::new(Vec::new()));
        let first_seen_for_handler = first_seen.clone();
        let first = broker
            .add_terminal_input_listener(Arc::new(move |input| {
                first_seen_for_handler
                    .lock()
                    .expect("first listener observations")
                    .push(input.to_string());
                Ok(Some(TerminalInputHandlerResult {
                    consume: Some(false),
                    data: Some(format!("{input}-first")),
                }))
            }))
            .expect("first terminal listener");
        let second_seen = Arc::new(Mutex::new(Vec::new()));
        let second_seen_for_handler = second_seen.clone();
        let second = broker
            .add_terminal_input_listener(Arc::new(move |input| {
                second_seen_for_handler
                    .lock()
                    .expect("second listener observations")
                    .push(input.to_string());
                Ok(Some(TerminalInputHandlerResult {
                    consume: Some(true),
                    data: Some(format!("{input}-second")),
                }))
            }))
            .expect("second terminal listener");

        assert_eq!(broker.terminal_input_listener_count(), 2);
        let dispatched = broker
            .dispatch_terminal_input("raw")
            .expect("terminal input dispatch");
        assert_eq!(dispatched.data, "raw-first-second");
        assert!(dispatched.consumed);
        assert_eq!(dispatched.listener_count, 2);
        assert_eq!(
            &*first_seen.lock().unwrap_or_else(|error| error.into_inner()),
            &vec!["raw".to_string()]
        );
        assert_eq!(
            &*second_seen
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            &vec!["raw-first".to_string()]
        );

        second.unsubscribe();
        assert_eq!(broker.terminal_input_listener_count(), 1);
        let dispatched = broker
            .dispatch_terminal_input("next")
            .expect("remaining terminal input dispatch");
        assert_eq!(dispatched.data, "next-first");
        assert!(!dispatched.consumed);
        drop(first);
        assert_eq!(broker.terminal_input_listener_count(), 0);
    }

    #[test]
    fn extension_ui_broker_diagnoses_terminal_listener_errors_and_panics() {
        let broker = ExtensionUiBroker::new();
        let _error = broker
            .add_terminal_input_listener(Arc::new(|_| Err("bad listener".to_string())))
            .expect("error listener");
        let _panic = broker
            .add_terminal_input_listener(Arc::new(|_| -> Result<_, String> {
                panic!("panic listener")
            }))
            .expect("panic listener");
        let result = broker
            .dispatch_terminal_input("input")
            .expect("dispatch continues after listener failures");
        assert_eq!(result.data, "input");
        assert_eq!(result.error_count, 2);
        let diagnostics = broker.drain_diagnostics();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "terminal_input_handler_error"
                && diagnostic.message.contains("bad listener")
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "terminal_input_handler_panic"
                && diagnostic.message.contains("panic listener")
        }));
    }

    #[test]
    fn extension_ui_broker_routes_custom_overlay_and_factory_result() {
        let broker = ExtensionUiBroker::new();
        let context = worker_ui_context(&broker);
        let factory_calls = Arc::new(Mutex::new(Vec::new()));
        let factory_calls_for_callback = factory_calls.clone();
        let factory = ExtensionUiFactory::Callback(Arc::new(move |request| {
            factory_calls_for_callback
                .lock()
                .expect("custom factory observations")
                .push(request.clone());
            Ok(serde_json::json!({
                "component": "custom",
                "surface": request.surface,
                "data": request.data,
            }))
        }));
        let worker = thread::spawn(move || {
            context.custom_with_options(
                factory,
                Some(serde_json::json!({"width": 40})),
                ExtensionUiDialogOptions {
                    timeout: Some(Duration::from_secs(1)),
                    cancel: None,
                },
            )
        });
        let request = next_ui_request(&broker);
        assert_eq!(request["method"], "custom");
        assert_eq!(request["options"], serde_json::json!({"width": 40}));
        assert_eq!(request["factory"]["component"], "custom");
        assert_eq!(request["factory"]["surface"], "custom");
        assert_eq!(
            broker.handle_response(&serde_json::json!({
                "type": "extension_ui_response",
                "id": request_id(&request),
                "result": {"choice": "ok"},
            })),
            ExtensionUiResponseDisposition::Resolved
        );
        assert_eq!(
            worker.join().expect("custom overlay worker"),
            Ok(Some(serde_json::json!({"choice": "ok"})))
        );
        let calls = factory_calls.lock().expect("custom factory calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].surface, "custom");
        assert_eq!(calls[0].data, serde_json::json!({"width": 40}));
    }

    #[test]
    fn extension_ui_broker_tracks_remaining_ui_surfaces_and_theme_state() {
        let broker = ExtensionUiBroker::new();
        let context = worker_ui_context(&broker);
        broker.set_themes(vec![
            serde_json::json!({"name": "dark", "path": "/themes/dark.json"}),
            serde_json::json!({"name": "light", "path": "/themes/light.json"}),
        ]);
        assert_eq!(context.get_all_themes().len(), 2);
        assert_eq!(
            context.get_theme("dark"),
            Some(serde_json::json!({"name": "dark", "path": "/themes/dark.json"}))
        );

        context
            .set_hidden_thinking_label(Some("Reasoning"))
            .expect("hidden thinking label");
        context
            .set_widget_factory(
                "details",
                Some(ExtensionUiFactory::Json(serde_json::json!({
                    "component": "details"
                }))),
                Some("belowEditor"),
            )
            .expect("widget factory");
        context
            .set_header(Some(ExtensionUiFactory::Json(
                serde_json::json!({"component": "header"}),
            )))
            .expect("header factory");
        context
            .set_footer(Some(ExtensionUiFactory::Json(
                serde_json::json!({"component": "footer"}),
            )))
            .expect("footer factory");
        context
            .add_autocomplete_provider(ExtensionUiFactory::Json(serde_json::json!({
                "provider": "files"
            })))
            .expect("autocomplete provider");
        context
            .set_editor_component(Some(ExtensionUiFactory::Json(
                serde_json::json!({"component": "editor"}),
            )))
            .expect("editor component");
        let theme_result = context
            .set_theme(serde_json::json!("dark"))
            .expect("theme request");
        assert_eq!(theme_result, ExtensionUiThemeResult::success());
        assert_eq!(
            context.theme(),
            Some(serde_json::json!({"name": "dark", "path": "/themes/dark.json"}))
        );
        assert_eq!(
            context.set_theme(serde_json::json!("missing")),
            Ok(ExtensionUiThemeResult::failure("Theme not found: missing"))
        );
        context
            .set_tools_expanded(true)
            .expect("tool expansion state");

        assert_eq!(
            context.get_hidden_thinking_label().as_deref(),
            Some("Reasoning")
        );
        assert_eq!(
            context.get_editor_component(),
            Some(serde_json::json!({"component": "editor"}))
        );
        assert_eq!(
            context.get_autocomplete_providers(),
            vec![serde_json::json!({"provider": "files"})]
        );
        assert!(context.get_tools_expanded());
        let snapshot = context.ui_state_snapshot();
        assert_eq!(snapshot["hiddenThinkingLabel"], "Reasoning");
        assert_eq!(snapshot["widgets"]["details"]["component"], "details");
        assert_eq!(snapshot["header"]["component"], "header");
        assert_eq!(snapshot["footer"]["component"], "footer");
        assert_eq!(snapshot["theme"]["name"], "dark");
        assert_eq!(snapshot["toolsExpanded"], true);

        let requests = broker.drain_requests();
        let methods: Vec<_> = requests
            .iter()
            .filter_map(|request| request["method"].as_str())
            .collect();
        for method in [
            "setHiddenThinkingLabel",
            "setWidget",
            "setHeader",
            "setFooter",
            "addAutocompleteProvider",
            "setEditorComponent",
            "setTheme",
            "setToolsExpanded",
        ] {
            assert!(methods.contains(&method), "missing UI method {method}");
        }
    }
}
