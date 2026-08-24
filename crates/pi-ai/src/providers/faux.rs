//! Faux provider — test/extensibility provider. Port of
//! `packages/ai/src/providers/faux.ts`. Behaves like a real provider but
//! returns scripted responses, with the upstream usage-estimation semantics
//! (prompt caching via session id, token estimates, deltas).

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::Model;
use crate::types::{
    now_ms, AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DeferredHandle,
    DoneReason, ErrorReason, JsonValue, SimpleStreamOptions, StopReason, Usage,
};
use futures_util::FutureExt;

pub const DEFAULT_API: &str = "faux";
pub const DEFAULT_PROVIDER: &str = "faux";
pub const DEFAULT_MODEL_ID: &str = "faux-1";
pub const DEFAULT_MODEL_NAME: &str = "Faux Model";
pub const DEFAULT_BASE_URL: &str = "http://localhost:0";
const DEFAULT_MIN_TOKEN_SIZE: usize = 3;
const DEFAULT_MAX_TOKEN_SIZE: usize = 5;

fn default_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: Default::default(),
    }
}

pub fn faux_text(text: impl Into<String>) -> ContentBlock {
    ContentBlock::text(text)
}

pub fn faux_thinking(thinking: impl Into<String>) -> ContentBlock {
    ContentBlock::thinking(thinking)
}

pub fn faux_tool_call(name: impl Into<String>, arguments: JsonValue) -> ContentBlock {
    ContentBlock::tool_call(random_id("tool"), name, arguments)
}

pub fn faux_assistant_message(
    content: Vec<ContentBlock>,
    options: FauxAssistantOptions,
) -> AssistantMessage {
    let mut m = AssistantMessage::new();
    m.set_api_provider_model(DEFAULT_API, DEFAULT_PROVIDER, DEFAULT_MODEL_ID);
    *m.content_mut() = content;
    m.set_usage(default_usage());
    m.set_stop_reason(options.stop_reason.unwrap_or(StopReason::Stop));
    if let Some(msg) = options.error_message {
        let AssistantMessage::Assistant { error_message, .. } = &mut m;
        *error_message = Some(msg);
    }
    m
}

#[derive(Default)]
pub struct FauxAssistantOptions {
    pub stop_reason: Option<StopReason>,
    pub error_message: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct FauxProviderState {
    pub call_count: u64,
    pub deferred_fetch_count: u64,
    pub cancelled_deferred: Vec<DeferredHandle>,
}

pub type FauxResponseFactory = dyn Fn(&Context, Option<&SimpleStreamOptions>, &FauxProviderState, &Model) -> AssistantMessage
    + Send
    + Sync;

/// Scripted response step: either a fixed message or a factory.
#[allow(clippy::large_enum_variant)]
pub enum FauxResponseStep {
    Message(AssistantMessage),
    Factory(Box<FauxResponseFactory>),
}

impl std::fmt::Debug for FauxResponseStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FauxResponseStep::Message(m) => write!(f, "Message({m:?})"),
            FauxResponseStep::Factory(_) => write!(f, "Factory"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub cost: Option<Cost>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

use crate::types::Cost;

#[derive(Debug, Clone, Default)]
pub struct RegisterFauxProviderOptions {
    pub api: Option<String>,
    pub provider: Option<String>,
    pub models: Option<Vec<FauxModelDefinition>>,
    pub deferred: Option<FauxDeferredOptions>,
    pub tokens_per_second: Option<f64>,
    pub token_size: Option<FauxTokenSize>,
}

#[derive(Debug, Clone, Default)]
pub struct FauxDeferredOptions {
    pub pending_fetches: Option<u32>,
    pub poll_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct FauxTokenSize {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

/// The faux provider core: scripted stream behavior over queued responses.
#[derive(Clone)]
pub struct FauxProviderCore {
    pub api: String,
    pub provider: String,
    pub models: Vec<Model>,
    pub state: Arc<Mutex<FauxProviderState>>,
    pending_responses: Arc<Mutex<VecDeque<FauxResponseStep>>>,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    prompt_cache: Arc<Mutex<BTreeMap<String, String>>>,
    deferred_responses: Arc<Mutex<BTreeMap<String, DeferredEntry>>>,
    deferred_options: FauxDeferredOptions,
    /// Per-core deterministic RNG state for token-chunk boundaries. Kept on
    /// the core (not a global static) so tests are order-independent; the
    /// LCG uses wrapping arithmetic so it can never overflow-panic.
    rng: Arc<AtomicU64>,
}

#[allow(dead_code)] // populated/resolved by the deferred path; consumer lands with fetch resolution
struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: Context,
    options: SimpleStreamOptions,
    model: Model,
    pending_fetches: u32,
    cancelled: bool,
    final_: Option<AssistantMessage>,
}

impl FauxProviderCore {
    pub fn new(options: &RegisterFauxProviderOptions) -> Self {
        let api = options
            .api
            .clone()
            .unwrap_or_else(|| random_id(DEFAULT_API));
        let provider = options
            .provider
            .clone()
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
        let min_token_size = options
            .token_size
            .as_ref()
            .and_then(|t| t.min)
            .unwrap_or(DEFAULT_MIN_TOKEN_SIZE)
            .clamp(
                1,
                options
                    .token_size
                    .as_ref()
                    .and_then(|t| t.max)
                    .unwrap_or(DEFAULT_MAX_TOKEN_SIZE),
            );
        let max_token_size = options
            .token_size
            .as_ref()
            .and_then(|t| t.max)
            .unwrap_or(DEFAULT_MAX_TOKEN_SIZE)
            .max(min_token_size);
        let models = match &options.models {
            Some(defs) if !defs.is_empty() => defs
                .iter()
                .map(|d| {
                    let mut m = Model::new(
                        &d.id,
                        d.name.clone().unwrap_or_else(|| d.id.clone()),
                        &api,
                        &provider,
                    );
                    m.base_url = DEFAULT_BASE_URL.to_string();
                    m.reasoning = d.reasoning.unwrap_or(false);
                    m.cost = d
                        .cost
                        .clone()
                        .map(crate::types::Cost::into_tiered)
                        .unwrap_or_default();
                    m.context_window = d.context_window.unwrap_or(128_000);
                    m.max_tokens = d.max_tokens.unwrap_or(16_384);
                    m
                })
                .collect(),
            _ => vec![{
                let mut m = Model::new(DEFAULT_MODEL_ID, DEFAULT_MODEL_NAME, &api, &provider);
                m.base_url = DEFAULT_BASE_URL.to_string();
                m.context_window = 128_000;
                m.max_tokens = 16_384;
                m
            }],
        };
        Self {
            api,
            provider,
            models,
            state: Arc::new(Mutex::new(FauxProviderState::default())),
            pending_responses: Arc::new(Mutex::new(VecDeque::new())),
            min_token_size,
            max_token_size,
            tokens_per_second: options.tokens_per_second,
            prompt_cache: Arc::new(Mutex::new(BTreeMap::new())),
            deferred_responses: Arc::new(Mutex::new(BTreeMap::new())),
            deferred_options: options.deferred.clone().unwrap_or_default(),
            rng: Arc::new(AtomicU64::new(0)),
        }
    }

    #[allow(dead_code)] // used by tests and future registry integration
    pub fn get_model(&self, model_id: Option<&str>) -> Option<&Model> {
        match model_id {
            None => self.models.first(),
            Some(id) => self.models.iter().find(|m| m.id == id),
        }
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        *self.pending_responses.lock().unwrap() = responses.into();
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.pending_responses.lock().unwrap().extend(responses);
    }

    pub fn get_pending_response_count(&self) -> usize {
        self.pending_responses.lock().unwrap().len()
    }

    pub fn stream(
        &self,
        request_model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let api = self.api.clone();
        let provider = self.provider.clone();
        let model_id = request_model.id.clone();
        let min_token_size = self.min_token_size;
        let max_token_size = self.max_token_size;
        let tokens_per_second = self.tokens_per_second;
        let prompt_cache = self.prompt_cache.clone();
        let deferred_options = self.deferred_options.clone();
        let deferred_responses = self.deferred_responses.clone();
        let state = self.state.clone();
        let pending = self.pending_responses.clone();

        let outer = AssistantMessageEventStream::new();
        let event_tx = outer
            .sender()
            .expect("fresh stream must have a live channel");
        self.state.lock().unwrap().call_count += 1;
        let context = context.clone();
        let options = options.cloned();
        let request_model = request_model.clone();
        let rng = self.rng.clone();
        let panic_api = api.clone();
        let panic_provider = provider.clone();
        let panic_model_id = model_id.clone();

        let panic_tx = event_tx.clone();
        let body = async move {
            let mut outer = StreamPusher {
                tx: event_tx,
                finished: false,
            };
            // Upstream awaits streamOptions?.onResponse?.({status:200,headers:{}}, model)
            // before servicing the queued step.
            if let Some(on_response) = options.as_ref().and_then(|o| o.base.on_response.clone()) {
                on_response(
                    &crate::types::ProviderResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                    },
                    &request_model,
                );
            }
            let step = pending.lock().unwrap().pop_front();
            match step {
                None => {
                    let mut message = create_error_message(
                        "No more faux responses queued",
                        &api,
                        &provider,
                        &model_id,
                    );
                    message =
                        with_usage_estimate(message, &context, options.as_ref(), &prompt_cache);
                    outer.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error_message: message.clone(),
                    });
                }
                Some(step) => {
                    if let Some(_deferred) = &options.as_ref().and_then(|o| o.deferred.clone()) {
                        let handle_obj = DeferredHandle {
                            provider: request_model.provider.clone(),
                            model_id: request_model.id.clone(),
                            api: request_model.api.clone(),
                            id: random_id("deferred"),
                            expires_at: None,
                            poll_after_ms: deferred_options.poll_after_ms,
                            data: None,
                        };
                        deferred_responses.lock().unwrap().insert(
                            handle_obj.id.clone(),
                            DeferredEntry {
                                handle: handle_obj.clone(),
                                step,
                                context: context.clone(),
                                options: options.clone().unwrap_or_default(),
                                model: request_model.clone(),
                                pending_fetches: deferred_options.pending_fetches.unwrap_or(0),
                                cancelled: false,
                                final_: None,
                            },
                        );
                        let deferred_message = create_deferred_message(&request_model, &handle_obj);
                        stream_with_deltas(
                            &mut outer,
                            deferred_message,
                            min_token_size,
                            max_token_size,
                            tokens_per_second,
                            &rng,
                            None,
                        )
                        .await;
                        return;
                    }
                    let message = match step {
                        FauxResponseStep::Message(m) => m,
                        FauxResponseStep::Factory(f) => f(
                            &context,
                            options.as_ref(),
                            &state.lock().unwrap(),
                            &request_model,
                        ),
                    };
                    let message =
                        with_usage_estimate(message, &context, options.as_ref(), &prompt_cache);
                    stream_with_deltas(
                        &mut outer,
                        message,
                        min_token_size,
                        max_token_size,
                        tokens_per_second,
                        &rng,
                        None,
                    )
                    .await;
                }
            }
        };
        let handle = tokio::spawn(std::panic::AssertUnwindSafe(body).catch_unwind().then(
            async move |result| {
                if let Err(panic) = result {
                    // Guarantee stream termination on producer panic (P2-D):
                    // merely dropping the sender would hang consumers because the
                    // returned stream holds its own live sender.
                    let detail = panic
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic payload");
                    let message = create_error_message(
                        &format!("faux provider panicked while streaming: {detail}"),
                        &panic_api,
                        &panic_provider,
                        &panic_model_id,
                    );
                    let mut sink = StreamPusher {
                        tx: panic_tx,
                        finished: false,
                    };
                    sink.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error_message: message,
                    });
                }
            },
        ));
        // Keep the JoinHandle alive for the lifetime of the stream. The task
        // is detached; push events flow through the unbounded channel.
        std::mem::forget(handle);
        outer
    }

    /// Build a stream for a previously-deferred response (the synchronous
    /// provider hook used by `Models::fetch_deferred`). Resolves the stored
    /// entry: while `pending_fetches > 0` the deferred message is re-emitted;
    /// otherwise the step is resolved and streamed.
    pub fn fetch_deferred_stream(
        &self,
        request_model: &Model,
        handle: &DeferredHandle,
        _options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let state = self.state.clone();
        let deferred_responses = self.deferred_responses.clone();
        let min_token_size = self.min_token_size;
        let max_token_size = self.max_token_size;
        let tokens_per_second = self.tokens_per_second;
        let prompt_cache = self.prompt_cache.clone();
        let rng = self.rng.clone();
        let request_model = request_model.clone();
        let handle = handle.clone();
        let api = self.api.clone();
        let provider = self.provider.clone();

        let outer = AssistantMessageEventStream::new();
        let tx = match outer.sender() {
            Some(t) => t,
            None => return outer,
        };
        tokio::spawn(Box::pin(async move {
            let mut pusher = crate::event_stream::StreamSinkAdapter::new(tx.clone());
            state.lock().unwrap().deferred_fetch_count += 1;
            let resolution = resolve_deferred_entry(
                &deferred_responses,
                &handle,
                &request_model,
                &state,
                &prompt_cache,
            );
            match resolution {
                Ok(message) => {
                    stream_with_deltas(
                        &mut pusher,
                        message,
                        min_token_size,
                        max_token_size,
                        tokens_per_second,
                        &rng,
                        None,
                    )
                    .await;
                }
                Err(e) => {
                    let err_message = create_error_message(&e, &api, &provider, &request_model.id);
                    pusher.push(crate::types::AssistantMessageEvent::Error {
                        reason: crate::types::ErrorReason::Error,
                        error_message: err_message.clone(),
                    });
                    pusher.end(Some(err_message));
                }
            }
        }));
        outer
    }

    /// Async compatibility wrapper retained for direct faux-core callers.
    pub async fn fetch_deferred(
        &self,
        request_model: &Model,
        handle: &DeferredHandle,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.fetch_deferred_stream(request_model, handle, options)
    }

    /// Cancel a deferred response (upstream faux `cancelDeferred`).
    pub async fn cancel_deferred(&self, handle: &DeferredHandle) -> Result<(), String> {
        let handle = handle.clone();
        self.state
            .lock()
            .unwrap()
            .cancelled_deferred
            .push(handle.clone());
        if let Some(entry) = self.deferred_responses.lock().unwrap().get_mut(&handle.id) {
            entry.cancelled = true;
        }
        Ok(())
    }
}

/// Resolve one deferred entry to its current message (synchronous; called
/// with the entry lock held by the caller. The factory step runs under the
/// lock — user-supplied, sync, and short-lived).
fn resolve_deferred_entry(
    deferred_responses: &Arc<Mutex<BTreeMap<String, DeferredEntry>>>,
    handle: &DeferredHandle,
    request_model: &Model,
    state: &Arc<Mutex<FauxProviderState>>,
    prompt_cache: &Arc<Mutex<BTreeMap<String, String>>>,
) -> Result<AssistantMessage, String> {
    let mut lock = deferred_responses.lock().unwrap();
    let entry = lock
        .get_mut(&handle.id)
        .ok_or_else(|| format!("Unknown faux deferred response: {}", handle.id))?;
    if entry.handle.provider != handle.provider
        || entry.handle.model_id != handle.model_id
        || entry.handle.api != handle.api
    {
        return Err(format!("Unknown faux deferred response: {}", handle.id));
    }
    if entry.cancelled {
        return Err(format!(
            "Faux deferred response was cancelled: {}",
            handle.id
        ));
    }
    if entry.pending_fetches > 0 {
        entry.pending_fetches -= 1;
        return Ok(create_deferred_message(request_model, &entry.handle));
    }
    if let Some(final_) = &entry.final_ {
        return Ok(final_.clone());
    }
    let state_snapshot = state.lock().unwrap().clone();
    let resolved = match &entry.step {
        FauxResponseStep::Message(m) => m.clone(),
        FauxResponseStep::Factory(f) => f(&entry.context, None, &state_snapshot, &entry.model),
    };
    let message = with_usage_estimate(resolved, &entry.context, Some(&entry.options), prompt_cache);
    entry.final_ = Some(message.clone());
    Ok(message)
}

/// Minimal push surface for the background producer task./// Minimal push surface for the background producer task.
struct StreamPusher {
    tx: tokio::sync::mpsc::UnboundedSender<AssistantMessageEvent>,
    finished: bool,
}

impl crate::event_stream::StreamSink for StreamPusher {
    fn push(&mut self, event: AssistantMessageEvent) {
        StreamPusher::push_inner(self, event)
    }
    fn end(&mut self, result: Option<AssistantMessage>) {
        StreamPusher::end_inner(self, result)
    }
}

impl StreamPusher {
    fn push_inner(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        ) {
            self.finished = true;
        }
        let _ = self.tx.send(event);
    }

    fn end_inner(&mut self, _result: Option<AssistantMessage>) {
        self.finished = true;
    }
}

fn estimate_tokens(text: &str) -> u64 {
    (text.len() as f64 / 4.0).ceil() as u64
}

fn random_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Base36-ish suffix for readability.
    let suffix = (now_ms().wrapping_add(n.wrapping_mul(2654435761))) % 0x3b9aca07;
    format!("{prefix}:{suffix:x}")
}

fn content_to_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.clone(),
            ContentBlock::Image {
                data, mime_type, ..
            } => format!("[image:{mime_type}:{}]", data.len()),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_content_to_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.clone(),
            ContentBlock::Thinking { thinking, .. } => thinking.clone(),
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                format!(
                    "{name}:{}",
                    serde_json::to_string(arguments).unwrap_or_default()
                )
            }
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_to_text(message: &crate::types::Message) -> String {
    match message {
        crate::types::Message::User(u) => match u.content() {
            crate::types::UserContentBody::String(s) => s.clone(),
            crate::types::UserContentBody::Blocks(b) => content_to_text(b),
        },
        crate::types::Message::Assistant(a) => assistant_content_to_text(a.content()),
        crate::types::Message::ToolResult(t) => {
            let mut parts = vec![t.tool_name().to_string()];
            for block in t.content() {
                parts.push(content_to_text(std::slice::from_ref(block)));
            }
            parts.join("\n")
        }
    }
}

fn serialize_context(context: &Context) -> String {
    let mut parts = Vec::new();
    if let Some(sp) = &context.system_prompt {
        parts.push(format!("system:{sp}"));
    }
    for message in &context.messages {
        parts.push(format!("{}:{}", message.role(), message_to_text(message)));
    }
    if !context.tools.is_empty() {
        parts.push(format!(
            "tools:{}",
            serde_json::to_string(&context.tools).unwrap_or_default()
        ));
    }
    parts.join("\n\n")
}

fn common_prefix_length(a: &str, b: &str) -> usize {
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(x, y)| x == y)
        .count()
}

fn with_usage_estimate(
    mut message: AssistantMessage,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    prompt_cache: &Arc<Mutex<BTreeMap<String, String>>>,
) -> AssistantMessage {
    let prompt_text = serialize_context(context);
    let prompt_tokens = estimate_tokens(&prompt_text);
    let output_tokens = estimate_tokens(&assistant_content_to_text(message.content()));
    let session_id = options.and_then(|o| o.base.session_id.clone());
    let cache_retention = options.and_then(|o| o.base.cache_retention.clone());

    let mut input = prompt_tokens as i64;
    let mut cache_read = 0i64;
    let mut cache_write = 0i64;

    if let Some(session_id) = session_id {
        if cache_retention.as_deref() != Some("none") {
            let mut cache = prompt_cache.lock().unwrap();
            if let Some(previous) = cache.get(&session_id) {
                let cached_chars = common_prefix_length(previous, &prompt_text);
                cache_read = estimate_tokens(&previous[..cached_chars]) as i64;
                cache_write = estimate_tokens(&prompt_text[cached_chars..]) as i64;
                input = (prompt_tokens as i64).saturating_sub(cache_read);
            } else {
                cache_write = prompt_tokens as i64;
            }
            cache.insert(session_id, prompt_text);
        }
    }

    let usage = Usage {
        input,
        output: output_tokens as i64,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output_tokens as i64 + cache_read + cache_write,
        cost: Default::default(),
    };
    message.set_usage(usage);
    message
}

async fn stream_with_deltas(
    stream: &mut (dyn StreamSink + Send),
    message: AssistantMessage,
    min_token_size: usize,
    max_token_size: usize,
    tokens_per_second: Option<f64>,
    rng: &AtomicU64,
    _signal: Option<()>,
) {
    let mut partial = message.clone();
    *partial.content_mut() = Vec::new();
    partial.set_stop_reason(StopReason::Pending);

    stream.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    for (index, block) in message.content().iter().enumerate() {
        match block {
            ContentBlock::Thinking { thinking, .. } => {
                partial.content_mut().push(ContentBlock::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                });
                stream.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_by_token_size(rng, thinking, min_token_size, max_token_size) {
                    let _ = delay_by_tokens(&chunk, tokens_per_second).await;
                    if let ContentBlock::Thinking { thinking, .. } =
                        &mut partial.content_mut()[index]
                    {
                        thinking.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Text { text, .. } => {
                partial.content_mut().push(ContentBlock::Text {
                    text: String::new(),
                    text_signature: None,
                });
                stream.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in split_by_token_size(rng, text, min_token_size, max_token_size) {
                    let _ = delay_by_tokens(&chunk, tokens_per_second).await;
                    if let ContentBlock::Text { text: slot, .. } = &mut partial.content_mut()[index]
                    {
                        slot.push_str(&chunk);
                    }
                    stream.push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                stream.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                partial.content_mut().push(ContentBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: JsonValue::Null,
                    thought_signature: None,
                    namespace: None,
                });
                stream.push(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                let args_json =
                    serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string());
                for chunk in split_by_token_size(rng, &args_json, min_token_size, max_token_size) {
                    let _ = delay_by_tokens(&chunk, tokens_per_second).await;
                    stream.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                // Final arguments assembled from delta accumulation.
                if let ContentBlock::ToolCall {
                    arguments: slot, ..
                } = &mut partial.content_mut()[index]
                {
                    *slot = arguments.clone();
                }
                stream.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: block.clone(),
                    partial: partial.clone(),
                });
            }
            _ => {}
        }
    }

    match message.stop_reason() {
        Some(StopReason::Error) | Some(StopReason::Aborted) => {
            let reason = if message.stop_reason() == Some(StopReason::Aborted) {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            };
            stream.push(AssistantMessageEvent::Error {
                reason,
                error_message: message.clone(),
            });
            stream.end(Some(message));
        }
        Some(reason) => {
            let done_reason = match reason {
                StopReason::Stop => DoneReason::Stop,
                StopReason::Length => DoneReason::Length,
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Deferred => DoneReason::Deferred,
                _ => DoneReason::Stop,
            };
            stream.push(AssistantMessageEvent::Done {
                reason: done_reason,
                message: message.clone(),
            });
            stream.end(Some(message));
        }
        None => {
            let error_message = create_error_message(
                "Faux response ended without a stop reason",
                message.api().unwrap_or(DEFAULT_API),
                message.provider().unwrap_or(DEFAULT_PROVIDER),
                message.model().unwrap_or(DEFAULT_MODEL_ID),
            );
            stream.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error_message: error_message.clone(),
            });
            stream.end(Some(error_message));
        }
    }
}

// Per-core helper matching upstream's splitStringByTokenSize. Upstream uses
// Math.random; the faux port uses a deterministic LCG seeded from the core's
// own counter (never a global static) so tests stay order-independent. The
// arithmetic wraps explicitly: non-wrapping u64 overflow PANICS under debug
// overflow checks, and a panic inside the spawned producer used to hang the
// consumer (P2-D) before the catch_unwind guard landed.
fn split_by_token_size(
    rng: &AtomicU64,
    text: &str,
    min_token_size: usize,
    max_token_size: usize,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let seed = rng.fetch_add(1, Ordering::Relaxed);
        let combined = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let rnd = (combined >> 11) as f64 / (1u64 << 53) as f64;
        let rnd = rnd.clamp(0.0, 1.0 - f64::EPSILON);
        let token_size =
            min_token_size + (rnd * (max_token_size - min_token_size + 1) as f64) as usize;
        let char_size = (token_size * 4).max(1);
        let end = (index + char_size).min(text.len());
        chunks.push(text[index..end].to_string());
        index = end;
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

async fn delay_by_tokens(_chunk: &str, _tokens_per_second: Option<f64>) {
    // When tokensPerSecond is absent upstream resolves on a microtask; we
    // resolve immediately. (Timed delays are only useful in tests that
    // explicitly set tokensPerSecond, which we do not yet expose.)
    tokio::task::yield_now().await;
}

#[cfg(test)]
mod deferred_fetch_tests {
    use super::*;
    use crate::types::{ContentBlock, DeferredOption, StopReason};

    fn deferred_core(pending_fetches: u32) -> FauxProviderCore {
        let core = FauxProviderCore::new(&RegisterFauxProviderOptions {
            deferred: Some(FauxDeferredOptions {
                pending_fetches: Some(pending_fetches),
                poll_after_ms: Some(5),
            }),
            ..Default::default()
        });
        core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
            vec![ContentBlock::text("deferred done")],
            FauxAssistantOptions::default(),
        ))]);
        core
    }

    #[tokio::test]
    async fn deferred_stream_returns_handle_then_fetch_resolves() {
        let core = deferred_core(1);
        let model = core.models.first().cloned().unwrap();
        let options = SimpleStreamOptions {
            deferred: Some(DeferredOption::Bool(true)),
            ..Default::default()
        };
        let stream = core.stream(&model, &crate::types::Context::default(), Some(&options));
        let msg = stream.for_each(|_| {}).await;
        assert_eq!(msg.stop_reason(), Some(StopReason::Deferred));
        let handle = msg.deferred().expect("deferred handle").clone();

        // Poll 1: pendingFetches=1 left after submission, so re-deferred.
        let poll1 = core.fetch_deferred(&model, &handle, None).await;
        let msg1 = poll1.for_each(|_| {}).await;
        assert_eq!(msg1.stop_reason(), Some(StopReason::Deferred));

        // Poll 2: final resolution.
        let poll2 = core.fetch_deferred(&model, &handle, None).await;
        let msg2 = poll2.for_each(|_| {}).await;
        assert_eq!(msg2.stop_reason(), Some(StopReason::Stop));
        assert!(
            msg2.content()
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "deferred done")),
            "final message should carry the resolved content"
        );
        assert_eq!(core.state.lock().unwrap().deferred_fetch_count, 2);
        assert!(core.state.lock().unwrap().cancelled_deferred.is_empty());
    }

    #[tokio::test]
    async fn unknown_and_cancelled_deferred_fetch_errors() {
        let core = deferred_core(0);
        let model = core.models.first().cloned().unwrap();
        let options = SimpleStreamOptions {
            deferred: Some(DeferredOption::Bool(true)),
            ..Default::default()
        };
        let stream = core.stream(&model, &crate::types::Context::default(), Some(&options));
        let msg = stream.for_each(|_| {}).await;
        let handle = msg.deferred().expect("deferred handle").clone();

        // Unknown handle -> error stream.
        let mut unknown = handle.clone();
        unknown.id = "nope".to_string();
        let err_stream = core.fetch_deferred(&model, &unknown, None).await;
        let err_msg = err_stream.for_each(|_| {}).await;
        assert!(
            err_msg
                .error_message()
                .unwrap_or("")
                .contains("Unknown faux deferred response"),
            "{:?}",
            err_msg.error_message()
        );

        // Cancel then fetch -> cancelled error.
        core.cancel_deferred(&handle).await.unwrap();
        assert_eq!(core.state.lock().unwrap().cancelled_deferred.len(), 1);
        let cancelled_stream = core.fetch_deferred(&model, &handle, None).await;
        let cancelled_msg = cancelled_stream.for_each(|_| {}).await;
        assert!(
            cancelled_msg
                .error_message()
                .unwrap_or("")
                .contains("cancelled"),
            "{:?}",
            cancelled_msg.error_message()
        );
    }
}

fn create_deferred_message(model: &Model, handle: &DeferredHandle) -> AssistantMessage {
    let mut m = AssistantMessage::new();
    m.set_api_provider_model(&model.api, &model.provider, &model.id);
    m.set_usage(default_usage());
    m.set_stop_reason(StopReason::Deferred);
    m.set_deferred(handle.clone());
    m
}

fn create_error_message(error: &str, api: &str, provider: &str, model: &str) -> AssistantMessage {
    let mut m = AssistantMessage::new();
    m.set_api_provider_model(api, provider, model);
    m.set_usage(default_usage());
    m.set_stop_reason(StopReason::Error);
    let AssistantMessage::Assistant { error_message, .. } = &mut m;
    *error_message = Some(error.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partial_json::parse_partial_json;
    use crate::types::Message;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn streams_text_with_deltas() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("Hello, world!")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let context = Context::default();
            let stream = core.stream(&model, &context, None);
            let (events, final_message) = stream.collect().await;
            assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
            assert!(events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::TextStart { .. })));
            assert!(events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));
            assert_eq!(final_message.stop_reason(), Some(StopReason::Stop));
            assert!(final_message
                .content()
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "Hello, world!")));
        });
    }

    #[test]
    fn usage_estimate_counts_prompt_once() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("hi")],
                    FauxAssistantOptions::default(),
                )),
                FauxResponseStep::Message(faux_assistant_message(
                    vec![ContentBlock::text("hi")],
                    FauxAssistantOptions::default(),
                )),
            ]);
            let model = core.get_model(None).unwrap().clone();
            let mut context = Context::default();
            context
                .messages
                .push(Message::User(crate::types::UserContent::string("hello", 1)));
            let opts = SimpleStreamOptions {
                base: crate::types::StreamOptions {
                    session_id: Some("s1".into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            let (_, m1) = core.stream(&model, &context, Some(&opts)).collect().await;
            let (_, m2) = core.stream(&model, &context, Some(&opts)).collect().await;
            let u1 = m1.usage().unwrap();
            let u2 = m2.usage().unwrap();
            // "Counts the prompt once": the first call charges the whole
            // prompt as input and writes the cache; the second call reads the
            // full prompt from cache (same session id, same context), so its
            // input is 0 and its cache_read equals the prompt token count.
            assert_eq!(u1.input, u1.cache_write); // first call writes all of it
            assert!(u1.cache_write > 0);
            assert!(u1.output > 0);
            assert_eq!(u2.input, 0);
            assert!(u2.cache_read > 0);
            assert_eq!(u2.cache_read, u1.cache_write); // full prefix cached
                                                       // total always decomposes (upstream contract).
            assert_eq!(
                u1.total_tokens,
                u1.input + u1.output + u1.cache_read + u1.cache_write
            );
            assert_eq!(
                u2.total_tokens,
                u2.input + u2.output + u2.cache_read + u2.cache_write
            );
        });
    }

    #[test]
    fn invokes_on_response_with_synthetic_200() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![ContentBlock::text("ok")],
                FauxAssistantOptions::default(),
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let invoked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let seen_status = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let invoked_cb = invoked.clone();
            let seen_cb = seen_status.clone();
            let opts = crate::types::SimpleStreamOptions {
                base: crate::types::StreamOptions {
                    on_response: Some(std::sync::Arc::new(move |resp, _model| {
                        invoked_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        seen_cb.store(resp.status as u64, std::sync::atomic::Ordering::SeqCst);
                    })),
                    ..Default::default()
                },
                ..Default::default()
            };
            let (_, m) = core
                .stream(&model, &Context::default(), Some(&opts))
                .collect()
                .await;
            assert_eq!(m.stop_reason(), Some(StopReason::Stop));
            assert_eq!(invoked.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(seen_status.load(std::sync::atomic::Ordering::SeqCst), 200);
        });
    }

    #[test]
    fn errors_when_no_responses_queued() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            let model = core.get_model(None).unwrap().clone();
            let (_, m) = core
                .stream(&model, &Context::default(), None)
                .collect()
                .await;
            assert_eq!(m.stop_reason(), Some(StopReason::Error));
            assert_eq!(m.error_message(), Some("No more faux responses queued"));
        });
    }

    #[test]
    fn streams_tool_calls() {
        let rt = rt();
        rt.block_on(async {
            let core = FauxProviderCore::new(&RegisterFauxProviderOptions::default());
            core.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
                vec![faux_tool_call("bash", serde_json::json!({"command": "ls"}))],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            ))]);
            let model = core.get_model(None).unwrap().clone();
            let (events, m) = core
                .stream(&model, &Context::default(), None)
                .collect()
                .await;
            assert!(events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::ToolCallStart { .. })));
            assert!(events
                .iter()
                .any(|e| matches!(e, AssistantMessageEvent::ToolCallDelta { .. })));
            assert_eq!(m.stop_reason(), Some(StopReason::ToolUse));
            let calls: Vec<_> = m
                .content()
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => Some((name.as_str(), arguments.clone())),
                    _ => None,
                })
                .collect();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "bash");
            assert_eq!(calls[0].1, serde_json::json!({"command": "ls"}));
        });
    }

    #[test]
    fn partial_json_parse_reused_for_tool_args() {
        // The agent layer reconciles streamed tool_call_delta into parseable
        // args; ensure the partial parser handles a progressively streamed body.
        let target = r#"{"command": "ls -la", "cwd": "/tmp"}"#;
        let mut parsed = serde_json::Value::Null;
        for end in 1..=target.len() {
            // Mid-stream fragments may be rejected (e.g. a lone `-`); the
            // agent layer reconciles with the previous best value.
            if let Ok(v) = parse_partial_json(&target[..end]) {
                parsed = v;
            }
        }
        assert_eq!(
            parsed,
            serde_json::json!({"command": "ls -la", "cwd": "/tmp"})
        );
    }
}
