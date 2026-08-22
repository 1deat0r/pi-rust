//! Proxy stream function — port of `packages/agent/src/proxy.ts`.
//!
//! Streams an assistant response through a remote proxy server instead of
//! calling an LLM provider directly. The server manages provider auth and
//! forwards to the upstream provider; delta events are sent with the
//! `partial` field stripped to reduce bandwidth, and this module
//! reconstructs the partial `AssistantMessage` client-side exactly like the
//! upstream implementation.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use pi_ai::event_stream::{AssistantMessageEventStream, StreamSink};
use pi_ai::model::Model;
use pi_ai::partial_json::parse_streaming_json;
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Cost, DoneReason, ErrorReason,
    JsonValue, SimpleStreamOptions, StopReason, ThinkingBudgets, Usage,
};
use tokio::sync::mpsc;

/// Server-sent proxy events (upstream `ProxyAssistantMessageEvent`).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyAssistantMessageEvent {
    Start,
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "contentSignature", default)]
        content_signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "contentSignature", default)]
        content_signature: Option<String>,
    },
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ContentBlock,
    },
    Done {
        reason: ProxyDoneReason,
        usage: JsonValue,
    },
    Error {
        reason: ProxyErrorReason,
        #[serde(rename = "errorMessage", default)]
        error_message: Option<String>,
        usage: JsonValue,
    },
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyDoneReason {
    Stop,
    Length,
    ToolUse,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyErrorReason {
    Aborted,
    Error,
}

/// Serializable stream-option subset forwarded to the proxy server
/// (upstream `ProxySerializableStreamOptions`).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySerializableStreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
}

/// Stream options for a proxy request (upstream `ProxyStreamOptions`).
#[derive(Debug, Clone, Default)]
pub struct ProxyStreamOptions {
    /// Local abort flag for the proxy request.
    pub signal: Option<Arc<AtomicBool>>,
    /// Auth token sent as `Authorization: Bearer <token>`.
    pub auth_token: String,
    /// Proxy server URL (e.g. "https://genai.example.com").
    pub proxy_url: String,
    /// Serializable subset forwarded to the proxy server.
    pub options: ProxySerializableStreamOptions,
}

impl ProxyStreamOptions {
    /// Build request options from a stream's base options (the agent-loop
    /// entry point mirrors upstream: `streamProxy(model, context, {...options})`).
    pub fn from_stream_options(
        options: Option<&SimpleStreamOptions>,
        auth_token: impl Into<String>,
        proxy_url: impl Into<String>,
        signal: Option<Arc<AtomicBool>>,
    ) -> Self {
        let base = options.map(|o| &o.base).cloned().unwrap_or_default();
        let mut serializable = ProxySerializableStreamOptions {
            temperature: base.temperature,
            sampling_params: base.sampling_params,
            max_tokens: base.max_tokens,
            cache_retention: base.cache_retention,
            session_id: base.session_id.clone(),
            headers: base.base.headers.clone().filter(|h| !h.is_empty()),
            metadata: base.metadata.clone(),
            transport: base.transport.as_ref().map(|t| serde_json::to_value(t).unwrap_or(JsonValue::Null)),
            max_retry_delay_ms: base.base.max_retry_delay_ms,
            ..Default::default()
        };
        if let Some(o) = options {
            serializable.reasoning = o.reasoning.as_ref().map(|r| serde_json::to_value(r).unwrap_or(JsonValue::Null));
            serializable.thinking_budgets = o.thinking_budgets.clone();
        }
        Self { signal, auth_token: auth_token.into(), proxy_url: proxy_url.into(), options: serializable }
    }
}


/// Stream function that proxies through a server instead of calling LLM
/// providers directly (upstream `streamProxy`).
pub fn stream_proxy(model: &Model, context: &Context, options: ProxyStreamOptions) -> AssistantMessageEventStream {
    let outer = AssistantMessageEventStream::new();
    let Some(event_tx) = outer.sender() else {
        return outer;
    };
    let model = model.clone();
    let context = context.clone();
    let proxy_url = options.proxy_url;
    let auth_token = options.auth_token;
    let serializable = options.options;
    let signal = options.signal.clone();

    let body = async move {
        let mut sink = ProxyStreamPusher { tx: event_tx, finished: false };
        let mut partial = init_partial(&model);
        let aborted = |signal: &Option<Arc<AtomicBool>>| -> bool {
            signal.as_ref().map(|s| s.load(Ordering::SeqCst)).unwrap_or(false)
        };

        let client = reqwest::Client::new();
        let context_json = serde_json::json!({
            "systemPrompt": context.system_prompt,
            "messages": serde_json::to_value(&context.messages).unwrap_or(JsonValue::Array(vec![])),
            "tools": serde_json::to_value(&context.tools).unwrap_or(JsonValue::Array(vec![])),
        });
        let body_json = serde_json::json!({ "model": model, "context": context_json, "options": serializable });

        let response = match client
            .post(format!("{proxy_url}/api/stream"))
            .bearer_auth(auth_token)
            .header("Content-Type", "application/json")
            .json(&body_json)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                finalize_error(&mut sink, &mut partial, aborted(&signal), format!("Proxy error: {e}"));
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let status_text = status.canonical_reason().unwrap_or("").to_string();
            let mut error_message = format!("Proxy error: {status} {status_text}");
            if let Ok(bytes) = response.bytes().await {
                if let Ok(json) = serde_json::from_slice::<JsonValue>(&bytes) {
                    if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
                        error_message = format!("Proxy error: {err}");
                    }
                }
            }
            finalize_error(&mut sink, &mut partial, aborted(&signal), error_message);
            return;
        }

        // Read the SSE body incrementally, splitting on '\n' like upstream.
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut tool_partials: BTreeMap<usize, String> = BTreeMap::new();
        loop {
            if aborted(&signal) {
                finalize_error(&mut sink, &mut partial, true, "Request aborted by user".to_string());
                return;
            }
            match stream.next().await {
                Some(Ok(chunk)) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(at) = buffer.find('\n') {
                        let head = buffer[..at].to_string();
                        buffer = buffer[at + 1..].to_string();
                        handle_sse_line(&head, &mut sink, &mut partial, &mut tool_partials);
                    }
                }
                Some(Err(e)) => {
                    let message = if aborted(&signal) {
                        "Request aborted by user".to_string()
                    } else {
                        format!("Proxy error: {e}")
                    };
                    finalize_error(&mut sink, &mut partial, aborted(&signal), message);
                    return;
                }
                None => break,
            }
        }
        if aborted(&signal) {
            finalize_error(&mut sink, &mut partial, true, "Request aborted by user".to_string());
            return;
        }
        // Trailing line without a newline.
        let trailing = buffer.trim_end().to_string();
        if !trailing.is_empty() {
            handle_sse_line(&trailing, &mut sink, &mut partial, &mut tool_partials);
        }
        // Normal completion: the terminal `done` event was already pushed by
        // process_proxy_event; upstream calls stream.end() without a final
        // message to close the channel.
        sink.end(None);
    };

    let handle = tokio::spawn(body);
    std::mem::forget(handle);
    outer
}

fn init_partial(model: &Model) -> AssistantMessage {
    let mut partial = AssistantMessage::new();
    partial.set_api_provider_model(model.api.as_str(), model.provider.as_str(), model.id.as_str());
    partial.set_usage(Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0, total: 0.0 },
    });
    partial
}

/// Handle one SSE line (`data: ...`).
fn handle_sse_line(
    line: &str,
    sink: &mut ProxyStreamPusher,
    partial: &mut AssistantMessage,
    tool_partials: &mut BTreeMap<usize, String>,
) {
    let Some(data) = line.strip_prefix("data:") else { return };
    let data = data.trim();
    if data.is_empty() {
        return;
    }
    let proxy_event: ProxyAssistantMessageEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(_) => return,
    };
    if let Some(event) = process_proxy_event(proxy_event, partial, tool_partials) {
        sink.push(event);
    }
}

/// Process a proxy event and update the partial message (upstream
/// `processProxyEvent`). Streaming tool-call JSON accumulates in
/// `tool_partials` keyed by content index, replacing upstream's
/// `partialJson` field on the block.
fn process_proxy_event(
    proxy_event: ProxyAssistantMessageEvent,
    partial: &mut AssistantMessage,
    tool_partials: &mut BTreeMap<usize, String>,
) -> Option<AssistantMessageEvent> {
    match proxy_event {
        ProxyAssistantMessageEvent::Start => Some(AssistantMessageEvent::Start { partial: partial.clone() }),
        ProxyAssistantMessageEvent::TextStart { content_index } => {
            ensure_len(partial, content_index);
            partial.content_mut()[content_index] = ContentBlock::Text { text: String::new(), text_signature: None };
            Some(AssistantMessageEvent::TextStart { content_index, partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::TextDelta { content_index, delta } => {
            let text = text_mut(partial, content_index)?;
            text.push_str(&delta);
            Some(AssistantMessageEvent::TextDelta { content_index, delta, partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::TextEnd { content_index, content_signature } => {
            let block = partial.content_mut().get_mut(content_index)?;
            match block {
                ContentBlock::Text { text, text_signature } => {
                    *text_signature = content_signature;
                    Some(AssistantMessageEvent::TextEnd { content_index, content: text.clone(), partial: partial.clone() })
                }
                _ => None,
            }
        }
        ProxyAssistantMessageEvent::ThinkingStart { content_index } => {
            ensure_len(partial, content_index);
            partial.content_mut()[content_index] = ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
            };
            Some(AssistantMessageEvent::ThinkingStart { content_index, partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::ThinkingDelta { content_index, delta } => {
            let thinking = thinking_mut(partial, content_index)?;
            thinking.push_str(&delta);
            Some(AssistantMessageEvent::ThinkingDelta { content_index, delta, partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::ThinkingEnd { content_index, content_signature } => {
            let block = partial.content_mut().get_mut(content_index)?;
            match block {
                ContentBlock::Thinking { thinking, thinking_signature, .. } => {
                    *thinking_signature = content_signature;
                    Some(AssistantMessageEvent::ThinkingEnd { content_index, content: thinking.clone(), partial: partial.clone() })
                }
                _ => None,
            }
        }
        ProxyAssistantMessageEvent::ToolCallStart { content_index, id, tool_name } => {
            ensure_len(partial, content_index);
            partial.content_mut()[content_index] = ContentBlock::tool_call(id, tool_name, serde_json::json!({}));
            tool_partials.insert(content_index, String::new());
            Some(AssistantMessageEvent::ToolCallStart { content_index, partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::ToolCallDelta { content_index, delta } => {
            let block = partial.content_mut().get_mut(content_index)?;
            let ContentBlock::ToolCall { arguments, .. } = block else { return None };
            let acc = tool_partials.entry(content_index).or_default();
            acc.push_str(&delta);
            let parsed = parse_streaming_json(acc);
            *arguments = if parsed.is_null() { serde_json::json!({}) } else { parsed };
            Some(AssistantMessageEvent::ToolCallDelta { content_index, delta, partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::ToolCallEnd { content_index, tool_call } => {
            let ContentBlock::ToolCall { id, name, .. } = &tool_call else { return None };
            let block = partial.content_mut().get_mut(content_index)?;
            let ContentBlock::ToolCall { arguments, .. } = block else { return None };
            let arguments = arguments.clone();
            *block = ContentBlock::tool_call(id.clone(), name.clone(), arguments);
            tool_partials.remove(&content_index);
            Some(AssistantMessageEvent::ToolCallEnd { content_index, tool_call: block.clone(), partial: partial.clone() })
        }
        ProxyAssistantMessageEvent::Done { reason, usage } => {
            partial.set_stop_reason(match reason {
                ProxyDoneReason::Stop => StopReason::Stop,
                ProxyDoneReason::Length => StopReason::Length,
                ProxyDoneReason::ToolUse => StopReason::ToolUse,
            });
            partial.set_usage(parse_usage(usage));
            let done_reason = match partial.stop_reason() {
                Some(StopReason::ToolUse) => DoneReason::ToolUse,
                Some(StopReason::Length) => DoneReason::Length,
                _ => DoneReason::Stop,
            };
            Some(AssistantMessageEvent::Done { reason: done_reason, message: partial.clone() })
        }
        ProxyAssistantMessageEvent::Error { reason, error_message, usage } => {
            let is_aborted = matches!(reason, ProxyErrorReason::Aborted);
            partial.set_stop_reason(if is_aborted { StopReason::Aborted } else { StopReason::Error });
            let AssistantMessage::Assistant { error_message: slot, .. } = partial;
            *slot = error_message;
            partial.set_usage(parse_usage(usage));
            Some(AssistantMessageEvent::Error {
                reason: if is_aborted { ErrorReason::Aborted } else { ErrorReason::Error },
                error_message: partial.clone(),
            })
        }
    }
}

fn text_mut(partial: &mut AssistantMessage, index: usize) -> Option<&mut String> {
    match partial.content_mut().get_mut(index)? {
        ContentBlock::Text { text, .. } => Some(text),
        _ => None,
    }
}

fn thinking_mut(partial: &mut AssistantMessage, index: usize) -> Option<&mut String> {
    match partial.content_mut().get_mut(index)? {
        ContentBlock::Thinking { thinking, .. } => Some(thinking),
        _ => None,
    }
}

fn ensure_len(partial: &mut AssistantMessage, index: usize) {
    while partial.content_len() <= index {
        partial.content_mut().push(ContentBlock::Text { text: String::new(), text_signature: None });
    }
}

fn parse_usage(value: JsonValue) -> Usage {
    serde_json::from_value(value).unwrap_or_default()
}

fn finalize_error(sink: &mut ProxyStreamPusher, partial: &mut AssistantMessage, aborted: bool, message: String) {
    partial.set_stop_reason(if aborted { StopReason::Aborted } else { StopReason::Error });
    let AssistantMessage::Assistant { error_message, .. } = partial;
    *error_message = Some(message);
    sink.push(AssistantMessageEvent::Error {
        reason: if aborted { ErrorReason::Aborted } else { ErrorReason::Error },
        error_message: partial.clone(),
    });
    sink.end(None);
}

/// Minimal push surface for the background producer task.
struct ProxyStreamPusher {
    tx: mpsc::UnboundedSender<AssistantMessageEvent>,
    finished: bool,
}

impl ProxyStreamPusher {
    fn push_inner(&mut self, event: AssistantMessageEvent) {
        if self.finished {
            return;
        }
        if matches!(event, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }) {
            self.finished = true;
        }
        let _ = self.tx.send(event);
    }
    fn end_inner(&mut self, _result: Option<AssistantMessage>) {
        self.finished = true;
    }
}

impl StreamSink for ProxyStreamPusher {
    fn push(&mut self, event: AssistantMessageEvent) {
        self.push_inner(event)
    }
    fn end(&mut self, result: Option<AssistantMessage>) {
        self.end_inner(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::Tool;

    fn sample_model() -> Model {
        Model {
            id: "faux-1".into(),
            name: "Faux Model".into(),
            api: "faux".into(),
            provider: "faux".into(),
            base_url: "http://localhost:0".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: Default::default(),
            context_window: 0,
            max_tokens: 0,
            sampling_params: None,
            headers: None,
            compat: None,
            authenticated: false,
        }
    }

    fn new_partial() -> AssistantMessage {
        let mut p = AssistantMessage::new();
        p.set_api_provider_model("faux", "faux", "faux-1");
        p
    }

    fn test_usage() -> JsonValue {
        serde_json::json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
            "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
    }

    #[test]
    fn text_events_reconstruct_partial() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        process_proxy_event(ProxyAssistantMessageEvent::TextStart { content_index: 0 }, &mut partial, &mut tool_partials);
        process_proxy_event(ProxyAssistantMessageEvent::TextDelta { content_index: 0, delta: "hel".into() }, &mut partial, &mut tool_partials);
        process_proxy_event(ProxyAssistantMessageEvent::TextDelta { content_index: 0, delta: "lo".into() }, &mut partial, &mut tool_partials);
        assert!(matches!(&partial.content()[0], ContentBlock::Text { text, .. } if text == "hello"));
    }

    #[test]
    fn done_sets_reason_and_usage() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let ev = process_proxy_event(ProxyAssistantMessageEvent::Done { reason: ProxyDoneReason::Stop, usage: test_usage() }, &mut partial, &mut tool_partials);
        assert!(matches!(ev, Some(AssistantMessageEvent::Done { reason: DoneReason::Stop, .. })));
        assert_eq!(partial.stop_reason(), Some(StopReason::Stop));
    }

    #[test]
    fn error_sets_error_message() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let ev = process_proxy_event(
            ProxyAssistantMessageEvent::Error { reason: ProxyErrorReason::Error, error_message: Some("boom".into()), usage: test_usage() },
            &mut partial,
            &mut tool_partials,
        );
        assert!(matches!(ev, Some(AssistantMessageEvent::Error { reason: ErrorReason::Error, .. })));
        assert_eq!(partial.stop_reason(), Some(StopReason::Error));
        assert_eq!(partial.error_message(), Some("boom"));
    }

    #[test]
    fn tool_call_deltas_reconstruct_arguments() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        process_proxy_event(ProxyAssistantMessageEvent::ToolCallStart { content_index: 0, id: "tc1".into(), tool_name: "bash".into() }, &mut partial, &mut tool_partials);
        process_proxy_event(ProxyAssistantMessageEvent::ToolCallDelta { content_index: 0, delta: "{\"command\": \"ls\"".into() }, &mut partial, &mut tool_partials);
        process_proxy_event(ProxyAssistantMessageEvent::ToolCallDelta { content_index: 0, delta: "}".into() }, &mut partial, &mut tool_partials);
        assert!(matches!(&partial.content()[0], ContentBlock::ToolCall { name, .. } if name == "bash"));
        assert!(matches!(&partial.content()[0], ContentBlock::ToolCall { arguments, .. } if arguments.get("command").and_then(|v| v.as_str()) == Some("ls")));
    }

    #[test]
    fn tool_call_end_replaces_block() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        process_proxy_event(ProxyAssistantMessageEvent::ToolCallStart { content_index: 0, id: "tc1".into(), tool_name: "bash".into() }, &mut partial, &mut tool_partials);
        process_proxy_event(ProxyAssistantMessageEvent::ToolCallDelta { content_index: 0, delta: "{\"command\":\"ls\"}".into() }, &mut partial, &mut tool_partials);
        process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallEnd { content_index: 0, tool_call: ContentBlock::tool_call("tc1", "bash", serde_json::json!({"command": "ls"})) },
            &mut partial,
            &mut tool_partials,
        );
        assert!(matches!(&partial.content()[0], ContentBlock::ToolCall { id, .. } if id == "tc1"));
        assert!(tool_partials.is_empty());
    }

    #[test]
    fn sse_line_parses_data_prefix() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = ProxyStreamPusher { tx, finished: false };
        handle_sse_line("data: {\"type\":\"start\"}", &mut sink, &mut partial, &mut tool_partials);
        handle_sse_line("data: {\"type\":\"text_start\",\"contentIndex\":0}", &mut sink, &mut partial, &mut tool_partials);
        handle_sse_line("data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"hi\"}", &mut sink, &mut partial, &mut tool_partials);
        assert!(matches!(&partial.content()[0], ContentBlock::Text { text, .. } if text == "hi"));
    }

    #[tokio::test]
    async fn stream_proxy_surfaces_error_for_unreachable_url() {
        let model = sample_model();
        let context = Context { system_prompt: None, messages: vec![], tools: Vec::<Tool>::new() };
        let opts = ProxyStreamOptions {
            signal: None,
            auth_token: "token".into(),
            proxy_url: "http://127.0.0.1:1".into(),
            options: Default::default(),
        };
        let stream = stream_proxy(&model, &context, opts);
        let (_, message) = stream.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert!(message.error_message().is_some());
    }
}
