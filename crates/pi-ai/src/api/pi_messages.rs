//! pi-messages API adaptor — port of
//! `packages/ai/src/api/pi-messages.ts`.
//!
//! Streams pi's own message protocol directly to a backend: a single POST of
//! `{ model, context, options }` to `<baseUrl>/messages`; the response is an
//! SSE stream of serialized assistant-message events plus a terminal
//! `done`/`error` event (the wire protocol spoken by the Radius gateway).
//!
//! DOCUMENTED DIVERGENCE: upstream attaches `pi_messages_rewrite` and
//! `pi_messages_response_failure` diagnostics to the assistant message. The
//! ported `AssistantMessage` type has no `diagnostics` field; the observable
//! error surfaces (message text, status/code composition) are preserved
//! verbatim. Seam marked `TODO(diagnostics)` below.

use serde_json::{json, Value};

use crate::event_stream::{AssistantMessageEventStream, StreamSink};
use crate::model::Model;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, ErrorReason,
    SimpleStreamOptions, StopReason, StreamOptions, Usage,
};

/// Options for pi-messages requests (subset of upstream `PiMessagesOptions`).
#[derive(Clone, Default)]
pub struct PiMessagesOptions {
    pub base: StreamOptions,
    pub reasoning: Option<String>,
    pub tool_choice: Option<Value>,
    /// Ask the backend for debug metadata (e.g. routing response headers).
    pub debug: bool,
}

fn create_empty_usage() -> Usage {
    Usage::default()
}

/// Parse a backend `usage` object into the unified `Usage`. The wire uses
/// camelCase cost fields (`cacheRead`/`cacheWrite`), while the Rust `Cost`
/// struct serializes snake_case, so build the struct field-by-field instead
/// of relying on serde rename symmetry.
fn parse_usage_value(value: &Value) -> Option<Usage> {
    let u64_field = |name: &str| value.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    let cost = value.get("cost");
    let f64_field = |name: &str| cost.and_then(|c| c.get(name)).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mut usage = Usage {
        input: u64_field("input"),
        output: u64_field("output"),
        cache_read: u64_field("cacheRead"),
        cache_write: u64_field("cacheWrite"),
        cache_write_1h: value.get("cacheWrite1h").and_then(|v| v.as_u64()),
        reasoning: value.get("reasoning").and_then(|v| v.as_u64()),
        total_tokens: u64_field("totalTokens"),
        cost: crate::types::Cost {
            input: f64_field("input"),
            output: f64_field("output"),
            cache_read: f64_field("cacheRead"),
            cache_write: f64_field("cacheWrite"),
            total: f64_field("total"),
        },
    };
    if usage.total_tokens == 0 {
        usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    }
    Some(usage)
}

/// Build the assistant message used for every event (upstream
/// `createEventConverter` partial).
struct EventConverter {
    partial: AssistantMessage,
    tool_json: std::collections::HashMap<usize, String>,
}

impl EventConverter {
    fn new(model: &Model) -> Self {
        let mut partial = AssistantMessage::new();
        partial.set_api_provider_model(&model.api, &model.provider, &model.id);
        partial.set_stop_reason(StopReason::Pending);
        Self { partial, tool_json: std::collections::HashMap::new() }
    }

    fn convert(&mut self, event: &Value) -> AssistantMessageEvent {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "done" => {
                let reason = event.get("reason").and_then(|v| v.as_str()).unwrap_or("stop");
                let dreason = done_reason(reason);
                if let Some(usage) = event.get("usage").and_then(parse_usage_value) {
                    self.partial.set_usage(usage);
                }
                if let Some(rid) = event.get("responseId").and_then(|v| v.as_str()) {
                    self.partial.set_response_id(rid.to_string());
                }
                self.partial.set_stop_reason(stop_reason_for_done(reason));
                AssistantMessageEvent::Done { reason: dreason, message: self.partial.clone() }
            }
            "error" => {
                let reason = event.get("reason").and_then(|v| v.as_str()).unwrap_or("error");
                if let Some(usage) = event.get("usage").and_then(parse_usage_value) {
                    self.partial.set_usage(usage);
                }
                if let Some(rid) = event.get("responseId").and_then(|v| v.as_str()) {
                    self.partial.set_response_id(rid.to_string());
                }
                let error_message = event.get("errorMessage").and_then(|v| v.as_str()).map(|s| s.to_string());
                let AssistantMessage::Assistant { error_message: slot, .. } = &mut self.partial;
                *slot = error_message;
                self.partial.set_stop_reason(if reason == "aborted" { StopReason::Aborted } else { StopReason::Error });
                AssistantMessageEvent::Error {
                    reason: if reason == "aborted" { ErrorReason::Aborted } else { ErrorReason::Error },
                    error_message: self.partial.clone(),
                }
            }
            "start" => AssistantMessageEvent::Start { partial: self.partial.clone() },
            "text_start" => {
                let index = content_index(event);
                set_content_at(&mut self.partial, index, ContentBlock::text(""));
                AssistantMessageEvent::TextStart { content_index: index, partial: self.partial.clone() }
            }
            "text_delta" => {
                let index = content_index(event);
                let delta = event.get("delta").and_then(|v| v.as_str()).unwrap_or("").to_string();
                append_text(&mut self.partial, index, &delta);
                AssistantMessageEvent::TextDelta { content_index: index, delta, partial: self.partial.clone() }
            }
            "text_end" => {
                let index = content_index(event);
                let content = event.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let signature = event.get("contentSignature").and_then(|v| v.as_str()).map(|s| s.to_string());
                set_text_end(&mut self.partial, index, content.clone(), signature);
                AssistantMessageEvent::TextEnd { content_index: index, content, partial: self.partial.clone() }
            }
            "thinking_start" => {
                let index = content_index(event);
                set_content_at(&mut self.partial, index, ContentBlock::thinking(""));
                AssistantMessageEvent::ThinkingStart { content_index: index, partial: self.partial.clone() }
            }
            "thinking_delta" => {
                let index = content_index(event);
                let delta = event.get("delta").and_then(|v| v.as_str()).unwrap_or("").to_string();
                append_thinking(&mut self.partial, index, &delta);
                AssistantMessageEvent::ThinkingDelta { content_index: index, delta, partial: self.partial.clone() }
            }
            "thinking_end" => {
                let index = content_index(event);
                let content = event.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let signature = event.get("contentSignature").and_then(|v| v.as_str()).map(|s| s.to_string());
                let redacted = event.get("redacted").and_then(|v| v.as_bool());
                set_thinking_end(&mut self.partial, index, content.clone(), signature, redacted);
                AssistantMessageEvent::ThinkingEnd { content_index: index, content, partial: self.partial.clone() }
            }
            "toolcall_start" => {
                let index = content_index(event);
                let id = event.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let tool_name = event.get("toolName").and_then(|v| v.as_str()).unwrap_or("").to_string();
                set_content_at(&mut self.partial, index, ContentBlock::tool_call(id, tool_name, json!({})));
                self.tool_json.insert(index, String::new());
                AssistantMessageEvent::ToolCallStart { content_index: index, partial: self.partial.clone() }
            }
            "toolcall_delta" => {
                let index = content_index(event);
                let delta = event.get("delta").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let json = format!("{}{}", self.tool_json.get(&index).map(String::as_str).unwrap_or(""), delta);
                self.tool_json.insert(index, json.clone());
                set_tool_arguments(&mut self.partial, index, &json);
                AssistantMessageEvent::ToolCallDelta { content_index: index, delta, partial: self.partial.clone() }
            }
            "toolcall_end" => {
                let index = content_index(event);
                let tool_call_value = event.get("toolCall").cloned().unwrap_or_else(|| json!({}));
                let id = tool_call_value.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = tool_call_value.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let arguments = tool_call_value.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let thought_signature = tool_call_value.get("thoughtSignature").and_then(|v| v.as_str()).map(|s| s.to_string());
                let namespace = tool_call_value.get("namespace").and_then(|v| v.as_str()).map(|s| s.to_string());
                set_content_at(&mut self.partial, index, ContentBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    thought_signature,
                    namespace,
                });
                self.tool_json.remove(&index);
                AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        thought_signature: None,
                        namespace: None,
                    },
                    partial: self.partial.clone(),
                }
            }
            _ => {
                // Unknown event type: mirror "start" passthrough semantics by
                // returning a start event (the partial is unchanged).
                AssistantMessageEvent::Start { partial: self.partial.clone() }
            }
        }
    }
}

fn content_index(event: &Value) -> usize {
    event.get("contentIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

fn set_content_at(msg: &mut AssistantMessage, index: usize, block: ContentBlock) {
    let content = msg.content_mut();
    if index >= content.len() {
        content.resize(index + 1, ContentBlock::text(""));
    }
    content[index] = block;
}

fn append_text(msg: &mut AssistantMessage, index: usize, delta: &str) {
    let content = msg.content_mut();
    if index >= content.len() {
        content.resize(index + 1, ContentBlock::text(""));
    }
    if let ContentBlock::Text { text, .. } = &mut content[index] {
        text.push_str(delta);
    }
}

fn append_thinking(msg: &mut AssistantMessage, index: usize, delta: &str) {
    let content = msg.content_mut();
    if index >= content.len() {
        content.resize(index + 1, ContentBlock::thinking(""));
    }
    if let ContentBlock::Thinking { thinking, .. } = &mut content[index] {
        thinking.push_str(delta);
    }
}

fn set_text_end(msg: &mut AssistantMessage, index: usize, text: String, signature: Option<String>) {
    let content = msg.content_mut();
    if index >= content.len() {
        content.resize(index + 1, ContentBlock::text(""));
    }
    content[index] = ContentBlock::Text { text, text_signature: signature };
}

fn set_thinking_end(
    msg: &mut AssistantMessage,
    index: usize,
    thinking: String,
    signature: Option<String>,
    redacted: Option<bool>,
) {
    let content = msg.content_mut();
    if index >= content.len() {
        content.resize(index + 1, ContentBlock::thinking(""));
    }
    content[index] = ContentBlock::Thinking {
        thinking,
        thinking_signature: signature,
        redacted,
    };
}

fn set_tool_arguments(msg: &mut AssistantMessage, index: usize, json_str: &str) {
    let parsed = crate::partial_json::parse_streaming_json(json_str);
    let content = msg.content_mut();
    if let Some(ContentBlock::ToolCall { arguments, .. }) = content.get_mut(index) {
        *arguments = parsed;
    }
}

fn done_reason(reason: &str) -> DoneReason {
    match reason {
        "length" => DoneReason::Length,
        "toolUse" => DoneReason::ToolUse,
        "deferred" => DoneReason::Deferred,
        _ => DoneReason::Stop,
    }
}

fn stop_reason_for_done(reason: &str) -> StopReason {
    match reason {
        "length" => StopReason::Length,
        "toolUse" => StopReason::ToolUse,
        "deferred" => StopReason::Deferred,
        _ => StopReason::Stop,
    }
}

/// Parse one buffered SSE event frame (upstream `parsePiMessagesEvent`): the
/// first `data:` line, `[DONE]` sentinel handling.
fn parse_pi_messages_event(raw: &str) -> Option<Value> {
    let data = raw
        .split('\n')
        .find(|line| line.starts_with("data:"))
        .map(|line| line[5..].trim())
        .filter(|s| !s.is_empty() && *s != "[DONE]")?;
    serde_json::from_str(data).ok()
}

/// Incremental SSE event reader for the pi-messages protocol (upstream
/// `readPiMessagesEvents`): normalizes `\r\n`, splits on blank lines, and
/// flushes any trailing buffered data at EOF.
fn read_pi_messages_events(bytes: impl Iterator<Item = Vec<u8>>) -> Vec<Value> {
    let mut buffer = String::new();
    let mut events = Vec::new();
    let mut pushed = false;
    for chunk in bytes {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        buffer = buffer.replace("\r\n", "\n");
        while let Some(split) = buffer.find("\n\n") {
            let raw = buffer[..split].to_string();
            if let Some(event) = parse_pi_messages_event(&raw) {
                events.push(event);
            }
            buffer = buffer[split + 2..].to_string();
            pushed = true;
        }
    }
    if buffer.trim() != "" {
        if let Some(event) = parse_pi_messages_event(&buffer) {
            events.push(event);
        }
    }
    let _ = pushed;
    events
}

/// Serialize the unified context into the upstream request `context`
/// shape (`systemPrompt`, `messages`, `tools`).
fn context_to_json(context: &Context) -> Value {
    let mut obj = serde_json::Map::new();
    if let Some(sp) = &context.system_prompt {
        obj.insert("systemPrompt".to_string(), Value::String(sp.clone()));
    }
    obj.insert("messages".to_string(), serde_json::to_value(&context.messages).unwrap_or(Value::Null));
    if !context.tools.is_empty() {
        obj.insert("tools".to_string(), serde_json::to_value(&context.tools).unwrap_or(Value::Null));
    }
    Value::Object(obj)
}

fn resolve_cache_retention(cache_retention: Option<&str>, env: Option<&crate::types::ProviderEnv>) -> Option<String> {
    if let Some(retention) = cache_retention {
        return Some(retention.to_string());
    }
    let val = env
        .and_then(|e| e.get("PI_CACHE_RETENTION").cloned())
        .or_else(|| std::env::var("PI_CACHE_RETENTION").ok())
        .filter(|v| !v.is_empty());
    if val.as_deref() == Some("long") {
        Some("long".to_string())
    } else {
        None
    }
}

/// Stream a request against a pi-messages backend.
pub fn stream(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &PiMessagesOptions,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let sender = match stream.sender() {
        Some(s) => s,
        None => return stream,
    };
    let model = model.clone();
    let context = context.clone();
    let options = options.clone();
    let api_key = api_key.map(|s| s.to_string());

    tokio::spawn(async move {
        let mut pusher = crate::event_stream::StreamSinkAdapter::new(sender);
        let mut converter = EventConverter::new(&model);

        let result = run(&model, &context, client, api_key.as_deref(), &options).await;
        match result {
            Ok(events) => {
                let mut terminal = false;
                for event in events {
                    let ev = converter.convert(&event);
                    let is_terminal = matches!(ev, AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. });
                    pusher.push(ev);
                    if is_terminal {
                        terminal = true;
                        break;
                    }
                }
                if !terminal {
                    let message = error_event_message(&model, format!(
                        "{} stream ended without a terminal event",
                        model.provider
                    ));
                    pusher.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error_message: message.clone(),
                    });
                    pusher.end(Some(message));
                }
            }
            Err(message) => {
               	let message = error_event_message(&model, message);
                pusher.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error_message: message.clone(),
                });
                pusher.end(Some(message));
            }
        }
    });
    stream
}

fn error_event_message(model: &Model, message: String) -> AssistantMessage {
    let mut msg = AssistantMessage::new();
    msg.set_api_provider_model(&model.api, &model.provider, &model.id);
    msg.set_stop_reason(StopReason::Error);
    msg.set_usage(create_empty_usage());
    let AssistantMessage::Assistant { error_message, .. } = &mut msg;
    *error_message = Some(message);
    msg
}

async fn run(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &PiMessagesOptions,
) -> Result<Vec<Value>, String> {
    let api_key = api_key.ok_or_else(|| format!("No API key provided for provider \"{}\"", model.provider))?;

    let mut url = format!("{}/messages", model.base_url.trim_end_matches('/'));
    if options.debug {
        url.push_str("?debug=1");
    }

    let mut payload = json!({
        "model": model.id,
        "context": context_to_json(context)
    });
    let mut inner = serde_json::Map::new();
    if let Some(t) = options.base.temperature {
        inner.insert("temperature".to_string(), json!(t));
    }
    if let Some(m) = options.base.max_tokens {
        inner.insert("maxTokens".to_string(), json!(m));
    }
    if let Some(r) = &options.reasoning {
        inner.insert("reasoning".to_string(), json!(r));
    }
    if let Some(c) = resolve_cache_retention(options.base.cache_retention.as_deref(), options.base.base.env.as_ref()) {
        inner.insert("cacheRetention".to_string(), json!(c));
    }
    if let Some(s) = &options.base.session_id {
        inner.insert("sessionId".to_string(), json!(s));
    }
    if let Some(tc) = &options.tool_choice {
        inner.insert("toolChoice".to_string(), tc.clone());
    }
    payload["options"] = Value::Object(inner);

    let mut request = client
        .post(&url)
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .bearer_auth(api_key)
        .json(&payload);
    if let Some(headers) = &options.base.base.headers {
        for (name, value) in headers {
            if let Some(value) = value {
                request = request.header(name.as_str(), value.as_str());
            }
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => return Err(format!("Request failed: {err}")),
    };
    let status = response.status();
    let headers_map = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if let Some(on_response) = &options.base.on_response {
        on_response(
            &crate::types::ProviderResponse { status: status.as_u16(), headers: headers_map },
            model,
        );
    }
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(err) => return Err(format!("Request body failed: {err}")),
    };

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&body).to_string();
        return Err(format_pi_messages_response_error(status.as_u16(), status.canonical_reason().unwrap_or(""), &body_text));
    }

    let chunks: Vec<Vec<u8>> = if body.is_empty() {
        Vec::new()
    } else {
        vec![body.to_vec()]
    };
    Ok(read_pi_messages_events(chunks.into_iter()))
}

/// Parse a backend error body and compose the upstream error surface:
/// `"<status> <statusText>: <message> (<code>)"` (upstream
/// `formatPiMessagesResponseError`).
fn format_pi_messages_response_error(status: u16, status_text: &str, body: &str) -> String {
    let error_body: Option<Value> = serde_json::from_str(body).ok().filter(|v: &Value| {
        v.get("error").map(|e| e.is_object()).unwrap_or(false)
    });
    let message = error_body
        .as_ref()
        .and_then(|v| v.get("error").and_then(|e| e.get("message")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let code = error_body
        .as_ref()
        .and_then(|v| v.get("error").and_then(|e| e.get("code")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let suffix = message.unwrap_or_else(|| body.to_string());
    let code_suffix = code.map(|c| format!(" ({c})")).unwrap_or_default();
    format!("{status} {status_text}: {suffix}{code_suffix}")
}

/// `streamSimple` — mirrors upstream by forwarding reasoning/toolChoice/debug
/// onto the full options.
pub fn stream_simple(
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    api_key: Option<&str>,
    options: &SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let full = PiMessagesOptions {
        base: options.base.clone(),
        reasoning: options.reasoning.map(|r| r.as_str().to_string()),
        tool_choice: options.tool_choice.map(|tc| match tc {
            crate::types::ToolChoice::Auto => json!("auto"),
            crate::types::ToolChoice::None => json!("none"),
        }),
        debug: false,
    };
    stream(model, context, client, api_key, &full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct RecordedRequest {
        url: String,
        headers: std::collections::BTreeMap<String, String>,
        body: Value,
    }

    /// Minimal local HTTP server: records one request and responds with the
    /// provided status/headers/SSE events.
    async fn start_server(
        status: u16,
        headers: &[(&str, &str)],
        events: &[Value],
        raw_body: Option<&str>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<RecordedRequest>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: std::sync::Arc<std::sync::Mutex<Vec<RecordedRequest>>> = Default::default();
        let requests_handle = requests.clone();
        let events = events.to_vec();
        let headers_owned: Vec<(String, String)> = headers.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let raw_body = raw_body.map(|s| s.to_string());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end: Option<usize> = None;
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                header_end = buf.windows(4).position(|w| w == b"\r\n\r\n");
                if header_end.is_some() {
                    break;
                }
            }
            let Some(header_end) = header_end else {
                return;
            };
            let text = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let mut lines = text.split("\r\n");
            let request_line = lines.next().unwrap_or("GET / HTTP/1.1");
            let mut req_headers = std::collections::BTreeMap::new();
            let mut content_length = 0usize;
            for line in lines {
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    req_headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            let body_start = header_end + 4;
            while buf.len() < body_start + content_length {
                let n = socket.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = if content_length > 0 {
                String::from_utf8_lossy(&buf[body_start..body_start + content_length]).trim().to_string()
            } else {
                String::new()
            };
            let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
            requests_handle.lock().unwrap().push(RecordedRequest {
                url: path,
                headers: req_headers,
                body: if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&body).unwrap_or(Value::Null)
                },
            });
            let resp_headers = headers_owned
                .iter()
                .map(|(k, v)| format!("{k}: {v}\r\n"))
                .collect::<String>();
            let body_out = if let Some(raw) = raw_body {
                raw.to_string()
            } else {
                events
                    .iter()
                    .map(|e| format!("data: {}\n\n", serde_json::to_string(e).unwrap()))
                    .collect()
            };
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 {status} {}\r\ncontent-type: {}\r\n{resp_headers}content-length: {}\r\nconnection: close\r\n\r\n{body_out}",
                    if status == 200 { "OK" } else { "Error" },
                    if status == 200 { "text/event-stream" } else { "application/json" },
                    body_out.len(),
                )
                .as_bytes(),
            ).await;
        });
        (format!("http://{addr}"), requests)
    }

    fn model(base_url: &str) -> Model {
        let mut m = Model::new("auto", "Radius Auto", "pi-messages", "radius");
        m.base_url = base_url.to_string();
        m
    }

    fn ctx() -> Context {
        Context {
            system_prompt: None,
            messages: vec![crate::types::Message::User(crate::types::UserContent::string("Hello", 1))],
            tools: vec![],
        }
    }

    fn usage_json() -> Value {
        json!({
            "input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15,
            "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3 }
        })
    }

    #[tokio::test]
    async fn streams_text_and_tool_calls_and_resolves_terminal_message() {
        let events = vec![
            json!({"type": "start"}),
            json!({"type": "text_start", "contentIndex": 0}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "Hel"}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "lo"}),
            json!({"type": "text_end", "contentIndex": 0, "content": "Hello"}),
            json!({"type": "toolcall_start", "contentIndex": 1, "id": "call_1", "toolName": "read"}),
            json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "{\"path\":"}),
            json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "\"a.txt\"}"}),
            json!({"type": "toolcall_end", "contentIndex": 1, "toolCall": {"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "a.txt"}}}),
            json!({"type": "done", "reason": "toolUse", "usage": usage_json(), "responseId": "resp_1"}),
        ];
        let (base_url, requests) = start_server(200, &[], &events, None).await;
        let model = model(&base_url);
        let options = PiMessagesOptions {
            base: StreamOptions {
                max_tokens: Some(100),
                session_id: Some("session-1".to_string()),
                base: crate::types::ProviderRequestOptions {
                    headers: Some({
                        let mut h = std::collections::BTreeMap::new();
                        h.insert("x-custom".to_string(), Some("1".to_string()));
                        h
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
            reasoning: None,
            tool_choice: Some(json!("auto")),
            debug: false,
        };
        let client = reqwest::Client::new();
        let s = stream(&model, &ctx(), client, Some("test-key"), &options);
        let (events_out, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
        assert_eq!(message.usage().map(|u| u.input), Some(10));
        assert_eq!(message.response_id(), Some("resp_1"));
        assert_eq!(message.model(), Some("auto"));
        assert_eq!(message.provider(), Some("radius"));
        assert_eq!(message.content().len(), 2);
        assert!(matches!(&message.content()[0], ContentBlock::Text { text, .. } if text == "Hello"));
        assert!(matches!(&message.content()[1], ContentBlock::ToolCall { id, name, .. } if id == "call_1" && name == "read"));
        assert!(events_out.iter().any(|e| matches!(e, AssistantMessageEvent::TextDelta { .. })));

        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].url, "/messages");
        assert_eq!(reqs[0].headers.get("authorization").map(|s| s.as_str()), Some("Bearer test-key"));
        assert_eq!(reqs[0].headers.get("x-custom").map(|s| s.as_str()), Some("1"));
        assert_eq!(reqs[0].body["model"], json!("auto"));
        assert_eq!(reqs[0].body["options"]["maxTokens"], json!(100));
        assert_eq!(reqs[0].body["options"]["sessionId"], json!("session-1"));
        assert_eq!(reqs[0].body["options"]["toolChoice"], json!("auto"));
    }

    #[tokio::test]
    async fn appends_debug_and_reports_response_headers() {
        let (base_url, requests) = start_server(200, &[("x-pi-gateway-upstream-provider", "anthropic")], &[json!({"type": "done", "reason": "stop", "usage": usage_json()})], None).await;
        let model = model(&base_url);
        let observed: std::sync::Arc<std::sync::Mutex<Option<crate::types::ProviderResponse>>> = Default::default();
        let observed2 = observed.clone();
        let options = PiMessagesOptions {
            base: StreamOptions {
                on_response: Some(std::sync::Arc::new(move |resp: &crate::types::ProviderResponse, _m: &Model| {
                    *observed2.lock().unwrap() = Some(resp.clone());
                })),
                ..Default::default()
            },
            reasoning: None,
            tool_choice: None,
            debug: true,
        };
        let client = reqwest::Client::new();
        let s = stream(&model, &ctx(), client, Some("test-key"), &options);
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        assert_eq!(requests.lock().unwrap()[0].url, "/messages?debug=1");
        let resp = observed.lock().unwrap().clone().unwrap();
        assert_eq!(resp.headers.get("x-pi-gateway-upstream-provider").map(|s| s.as_str()), Some("anthropic"));
    }

    #[tokio::test]
    async fn surfaces_backend_error_responses() {
        let body = json!({"error": {"message": "Token expired", "code": "unauthorized"}}).to_string();
        let (base_url, _) = start_server(401, &[], &[], Some(&body)).await;
        let model = model(&base_url);
        let client = reqwest::Client::new();
        let s = stream(&model, &ctx(), client, Some("stale"), &PiMessagesOptions::default());
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        let err = message.error_message().unwrap_or("");
        assert!(err.starts_with("401 "), "got: {err}");
        assert!(err.contains("Token expired"), "got: {err}");
        assert!(err.contains("unauthorized"), "got: {err}");
    }

    #[tokio::test]
    async fn propagates_server_sent_error_events() {
        let (base_url, _) = start_server(200, &[], &[
            json!({"type": "start"}),
            json!({"type": "error", "reason": "error", "usage": usage_json(), "errorMessage": "Upstream failed"}),
        ], None).await;
        let model = model(&base_url);
        let client = reqwest::Client::new();
        let s = stream(&model, &ctx(), client, Some("test-key"), &PiMessagesOptions::default());
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert_eq!(message.error_message(), Some("Upstream failed"));
        assert_eq!(message.usage().map(|u| u.output), Some(5));
    }

    #[tokio::test]
    async fn errors_when_no_api_key() {
        let model = model("http://127.0.0.1:1");
        let client = reqwest::Client::new();
        let s = stream(&model, &ctx(), client, None, &PiMessagesOptions::default());
        let (_, message) = s.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert!(message.error_message().unwrap_or("").contains("No API key provided"));
    }

    #[tokio::test]
    async fn errors_when_stream_ends_without_terminal_event() {
        let (base_url, _) = start_server(200, &[], &[
            json!({"type": "start"}),
            json!({"type": "text_start", "contentIndex": 0}),
            json!({"type": "text_delta", "contentIndex": 0, "delta": "partial"}),
        ], None).await;
        let model = model(&base_url);
        let client = reqwest::Client::new();
        let s = stream(&model, &ctx(), client, Some("test-key"), &PiMessagesOptions::default());
        let (_, message) = s.collect().await;
        assert!(message.error_message().unwrap_or("").contains("stream ended without a terminal event"));
    }

    #[test]
    fn parses_sse_frames_and_ignores_done_sentinel() {
        let chunks = vec![
            b"data: {\"type\": \"start\"}\n\ndata: {\"type\": \"text_delta\", \"contentIndex\": 0, \"delta\": \"hi\"}\r\n\r\ndata: [DONE]\n\n".to_vec(),
        ];
        let events = read_pi_messages_events(chunks.into_iter());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], json!("start"));
        assert_eq!(events[1]["type"], json!("text_delta"));
    }
}
