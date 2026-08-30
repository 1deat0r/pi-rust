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
use std::time::Duration;

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
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
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
pub enum ProxyDoneReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
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
            transport: base
                .transport
                .as_ref()
                .map(|t| serde_json::to_value(t).unwrap_or(JsonValue::Null)),
            max_retry_delay_ms: base.base.max_retry_delay_ms,
            ..Default::default()
        };
        if let Some(o) = options {
            serializable.reasoning = o
                .reasoning
                .as_ref()
                .map(|r| serde_json::to_value(r).unwrap_or(JsonValue::Null));
            serializable.thinking_budgets = o.thinking_budgets.clone();
        }
        Self {
            signal,
            auth_token: auth_token.into(),
            proxy_url: proxy_url.into(),
            options: serializable,
        }
    }
}

/// Stream function that proxies through a server instead of calling LLM
/// providers directly (upstream `streamProxy`).
pub fn stream_proxy(
    model: &Model,
    context: &Context,
    options: ProxyStreamOptions,
) -> AssistantMessageEventStream {
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
        let mut sink = ProxyStreamPusher {
            tx: event_tx,
            finished: false,
        };
        let mut partial = init_partial(&model);
        let aborted = |signal: &Option<Arc<AtomicBool>>| -> bool {
            signal
                .as_ref()
                .map(|s| s.load(Ordering::SeqCst))
                .unwrap_or(false)
        };

        let client = reqwest::Client::new();
        let context_json = serde_json::json!({
            "systemPrompt": context.system_prompt,
            "messages": serde_json::to_value(&context.messages).unwrap_or(JsonValue::Array(vec![])),
            "tools": serde_json::to_value(&context.tools).unwrap_or(JsonValue::Array(vec![])),
        });
        let body_json =
            serde_json::json!({ "model": model, "context": context_json, "options": serializable });

        if aborted(&signal) {
            finalize_error(
                &mut sink,
                &mut partial,
                true,
                "Request aborted by user".to_string(),
            );
            return;
        }

        let request = client
            .post(format!("{proxy_url}/api/stream"))
            .bearer_auth(auth_token)
            .header("Content-Type", "application/json")
            .json(&body_json);
        let response = if let Some(signal) = signal.clone() {
            tokio::select! {
                response = request.send() => response,
                _ = wait_for_proxy_abort(signal) => {
                    finalize_error(
                        &mut sink,
                        &mut partial,
                        true,
                        "Request aborted by user".to_string(),
                    );
                    return;
                }
            }
        } else {
            request.send().await
        };
        let response = match response {
            Ok(resp) => resp,
            Err(e) => {
                finalize_error(
                    &mut sink,
                    &mut partial,
                    aborted(&signal),
                    format!("Proxy error: {e}"),
                );
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
        let mut utf8_carry = Vec::new();
        let mut tool_partials: BTreeMap<usize, String> = BTreeMap::new();
        loop {
            if aborted(&signal) {
                finalize_error(
                    &mut sink,
                    &mut partial,
                    true,
                    "Request aborted by user".to_string(),
                );
                return;
            }
            let next_chunk = if let Some(signal) = signal.clone() {
                tokio::select! {
                    chunk = stream.next() => chunk,
                    _ = wait_for_proxy_abort(signal) => {
                        finalize_error(
                            &mut sink,
                            &mut partial,
                            true,
                            "Request aborted by user".to_string(),
                        );
                        return;
                    }
                }
            } else {
                stream.next().await
            };
            match next_chunk {
                Some(Ok(chunk)) => {
                    buffer.push_str(&decode_proxy_utf8_chunk(&mut utf8_carry, &chunk));
                    while let Some(at) = buffer.find('\n') {
                        let head = buffer[..at].to_string();
                        buffer = buffer[at + 1..].to_string();
                        if let Err(error) =
                            handle_sse_line(&head, &mut sink, &mut partial, &mut tool_partials)
                        {
                            finalize_error(&mut sink, &mut partial, aborted(&signal), error);
                            return;
                        }
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
            finalize_error(
                &mut sink,
                &mut partial,
                true,
                "Request aborted by user".to_string(),
            );
            return;
        }
        // Trailing line without a newline.
        buffer.push_str(&finish_proxy_utf8(&mut utf8_carry, String::new()));
        let trailing = buffer.trim_end().to_string();
        if !trailing.is_empty() {
            if let Err(error) =
                handle_sse_line(&trailing, &mut sink, &mut partial, &mut tool_partials)
            {
                finalize_error(&mut sink, &mut partial, aborted(&signal), error);
                return;
            }
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
    partial.set_api_provider_model(
        model.api.as_str(),
        model.provider.as_str(),
        model.id.as_str(),
    );
    partial.set_usage(Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: Cost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    });
    partial
}

/// Handle one SSE line (`data: ...`).
fn handle_sse_line(
    line: &str,
    sink: &mut ProxyStreamPusher,
    partial: &mut AssistantMessage,
    tool_partials: &mut BTreeMap<usize, String>,
) -> Result<(), String> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let data = data.trim();
    if data.is_empty() {
        return Ok(());
    }
    let proxy_event: ProxyAssistantMessageEvent =
        serde_json::from_str(data).map_err(|error| format!("Invalid proxy event: {error}"))?;
    if let Some(event) = process_proxy_event(proxy_event, partial, tool_partials)? {
        sink.push(event);
    }
    Ok(())
}

/// Process a proxy event and update the partial message (upstream
/// `processProxyEvent`). Streaming tool-call JSON accumulates in
/// `tool_partials` keyed by content index, replacing upstream's
/// `partialJson` field on the block.
fn process_proxy_event(
    proxy_event: ProxyAssistantMessageEvent,
    partial: &mut AssistantMessage,
    tool_partials: &mut BTreeMap<usize, String>,
) -> Result<Option<AssistantMessageEvent>, String> {
    match proxy_event {
        ProxyAssistantMessageEvent::Start => Ok(Some(AssistantMessageEvent::Start {
            partial: partial.clone(),
        })),
        ProxyAssistantMessageEvent::TextStart { content_index } => {
            ensure_len(partial, content_index);
            partial.content_mut()[content_index] = ContentBlock::Text {
                text: String::new(),
                text_signature: None,
            };
            Ok(Some(AssistantMessageEvent::TextStart {
                content_index,
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::TextDelta {
            content_index,
            delta,
        } => {
            let text = text_mut(partial, content_index)
                .ok_or_else(|| "Received text_delta for non-text content".to_string())?;
            text.push_str(&delta);
            Ok(Some(AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::TextEnd {
            content_index,
            content_signature,
        } => {
            let block = partial
                .content_mut()
                .get_mut(content_index)
                .ok_or_else(|| "Received text_end for non-text content".to_string())?;
            match block {
                ContentBlock::Text {
                    text,
                    text_signature,
                } => {
                    *text_signature = content_signature;
                    Ok(Some(AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text.clone(),
                        partial: partial.clone(),
                    }))
                }
                _ => Err("Received text_end for non-text content".to_string()),
            }
        }
        ProxyAssistantMessageEvent::ThinkingStart { content_index } => {
            ensure_len(partial, content_index);
            partial.content_mut()[content_index] = ContentBlock::Thinking {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
            };
            Ok(Some(AssistantMessageEvent::ThinkingStart {
                content_index,
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
        } => {
            let thinking = thinking_mut(partial, content_index)
                .ok_or_else(|| "Received thinking_delta for non-thinking content".to_string())?;
            thinking.push_str(&delta);
            Ok(Some(AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::ThinkingEnd {
            content_index,
            content_signature,
        } => {
            let block = partial
                .content_mut()
                .get_mut(content_index)
                .ok_or_else(|| "Received thinking_end for non-thinking content".to_string())?;
            match block {
                ContentBlock::Thinking {
                    thinking,
                    thinking_signature,
                    ..
                } => {
                    *thinking_signature = content_signature;
                    Ok(Some(AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content: thinking.clone(),
                        partial: partial.clone(),
                    }))
                }
                _ => Err("Received thinking_end for non-thinking content".to_string()),
            }
        }
        ProxyAssistantMessageEvent::ToolCallStart {
            content_index,
            id,
            tool_name,
        } => {
            ensure_len(partial, content_index);
            partial.content_mut()[content_index] =
                ContentBlock::tool_call(id, tool_name, serde_json::json!({}));
            tool_partials.insert(content_index, String::new());
            Ok(Some(AssistantMessageEvent::ToolCallStart {
                content_index,
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
        } => {
            let block = partial
                .content_mut()
                .get_mut(content_index)
                .ok_or_else(|| "Received toolcall_delta for non-toolCall content".to_string())?;
            let ContentBlock::ToolCall { arguments, .. } = block else {
                return Err("Received toolcall_delta for non-toolCall content".to_string());
            };
            let acc = tool_partials.entry(content_index).or_default();
            acc.push_str(&delta);
            let parsed = parse_streaming_json(acc);
            *arguments = if parsed.is_null() {
                serde_json::json!({})
            } else {
                parsed
            };
            Ok(Some(AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::ToolCallEnd {
            content_index,
            tool_call,
        } => {
            if !matches!(tool_call, ContentBlock::ToolCall { .. }) {
                return Ok(None);
            }
            let Some(block) = partial.content_mut().get_mut(content_index) else {
                return Ok(None);
            };
            if !matches!(block, ContentBlock::ToolCall { .. }) {
                return Ok(None);
            }
            // The terminal event is authoritative. The streaming deltas are
            // only a progressively parsed preview; mirror the upstream
            // Object.assign(toolCall) behavior instead of retaining a stale
            // id/name/arguments value from the preview block.
            *block = tool_call;
            tool_partials.remove(&content_index);
            Ok(Some(AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call: block.clone(),
                partial: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::Done { reason, usage } => {
            partial.set_stop_reason(match reason {
                ProxyDoneReason::Stop => StopReason::Stop,
                ProxyDoneReason::Length => StopReason::Length,
                ProxyDoneReason::ToolUse => StopReason::ToolUse,
            });
            partial.set_usage(parse_usage(usage)?);
            let done_reason = match partial.stop_reason() {
                Some(StopReason::ToolUse) => DoneReason::ToolUse,
                Some(StopReason::Length) => DoneReason::Length,
                _ => DoneReason::Stop,
            };
            Ok(Some(AssistantMessageEvent::Done {
                reason: done_reason,
                message: partial.clone(),
            }))
        }
        ProxyAssistantMessageEvent::Error {
            reason,
            error_message,
            usage,
        } => {
            let is_aborted = matches!(reason, ProxyErrorReason::Aborted);
            partial.set_stop_reason(if is_aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            });
            let AssistantMessage::Assistant {
                error_message: slot,
                ..
            } = partial;
            *slot = error_message;
            partial.set_usage(parse_usage(usage)?);
            Ok(Some(AssistantMessageEvent::Error {
                reason: if is_aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                },
                error_message: partial.clone(),
            }))
        }
    }
}

/// Decode HTTP body chunks like the browser `TextDecoder` used by the
/// upstream proxy client: a multibyte code point split between chunks must
/// survive until the following chunk arrives.
fn decode_proxy_utf8_chunk(carry: &mut Vec<u8>, bytes: &[u8]) -> String {
    carry.extend_from_slice(bytes);
    match std::str::from_utf8(carry) {
        Ok(text) => {
            let decoded = text.to_string();
            carry.clear();
            decoded
        }
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            let decoded = String::from_utf8_lossy(&carry[..valid]).into_owned();
            *carry = carry[valid..].to_vec();
            decoded
        }
        Err(_) => {
            let decoded = String::from_utf8_lossy(carry).into_owned();
            carry.clear();
            decoded
        }
    }
}

fn finish_proxy_utf8(carry: &mut Vec<u8>, mut decoded: String) -> String {
    if !carry.is_empty() {
        decoded.push_str(&String::from_utf8_lossy(carry));
        carry.clear();
    }
    decoded
}

async fn wait_for_proxy_abort(signal: Arc<AtomicBool>) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(1)).await;
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
        partial.content_mut().push(ContentBlock::Text {
            text: String::new(),
            text_signature: None,
        });
    }
}

fn parse_usage(value: JsonValue) -> Result<Usage, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Invalid proxy usage: expected an object".to_string())?;
    let cost = object
        .get("cost")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "Invalid proxy usage: missing cost object".to_string())?;
    Ok(Usage {
        input: wire_i64(object, "input")?,
        output: wire_i64(object, "output")?,
        cache_read: wire_i64_alias(object, "cacheRead", "cache_read")?,
        cache_write: wire_i64_alias(object, "cacheWrite", "cache_write")?,
        cache_write_1h: wire_optional_i64_alias(object, "cacheWrite1h", "cache_write_1h")?,
        reasoning: wire_optional_i64(object, "reasoning")?,
        total_tokens: wire_i64_alias(object, "totalTokens", "total_tokens")?,
        cost: Cost {
            input: wire_f64(cost, "input")?,
            output: wire_f64(cost, "output")?,
            cache_read: wire_f64_alias(cost, "cacheRead", "cache_read")?,
            cache_write: wire_f64_alias(cost, "cacheWrite", "cache_write")?,
            total: wire_f64(cost, "total")?,
        },
    })
}

fn wire_i64(object: &serde_json::Map<String, JsonValue>, field: &str) -> Result<i64, String> {
    let Some(value) = object.get(field) else {
        return Ok(0);
    };
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .ok_or_else(|| format!("Invalid proxy usage: {field} must be an integer"))
}

fn wire_i64_alias(
    object: &serde_json::Map<String, JsonValue>,
    primary: &str,
    alias: &str,
) -> Result<i64, String> {
    object
        .get(primary)
        .or_else(|| object.get(alias))
        .map_or(Ok(0), |value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
                .ok_or_else(|| format!("Invalid proxy usage: {primary} must be an integer"))
        })
}

fn wire_optional_i64(
    object: &serde_json::Map<String, JsonValue>,
    field: &str,
) -> Result<Option<i64>, String> {
    object
        .get(field)
        .map_or(Ok(None), |_value| wire_i64(object, field).map(Some))
}

fn wire_optional_i64_alias(
    object: &serde_json::Map<String, JsonValue>,
    primary: &str,
    alias: &str,
) -> Result<Option<i64>, String> {
    object
        .get(primary)
        .or_else(|| object.get(alias))
        .map_or(Ok(None), |value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
                .map(Some)
                .ok_or_else(|| format!("Invalid proxy usage: {primary} must be an integer"))
        })
}

fn wire_f64(object: &serde_json::Map<String, JsonValue>, field: &str) -> Result<f64, String> {
    let Some(value) = object.get(field) else {
        return Ok(0.0);
    };
    let number = value
        .as_f64()
        .ok_or_else(|| format!("Invalid proxy usage: {field} must be a number"))?;
    if number.is_finite() {
        Ok(number)
    } else {
        Err(format!("Invalid proxy usage: {field} must be finite"))
    }
}

fn wire_f64_alias(
    object: &serde_json::Map<String, JsonValue>,
    primary: &str,
    alias: &str,
) -> Result<f64, String> {
    object
        .get(primary)
        .or_else(|| object.get(alias))
        .map_or(Ok(0.0), |value| {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("Invalid proxy usage: {primary} must be a number"))?;
            if number.is_finite() {
                Ok(number)
            } else {
                Err(format!("Invalid proxy usage: {primary} must be finite"))
            }
        })
}

fn finalize_error(
    sink: &mut ProxyStreamPusher,
    partial: &mut AssistantMessage,
    aborted: bool,
    message: String,
) {
    partial.set_stop_reason(if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    });
    let AssistantMessage::Assistant { error_message, .. } = partial;
    *error_message = Some(message);
    sink.push(AssistantMessageEvent::Error {
        reason: if aborted {
            ErrorReason::Aborted
        } else {
            ErrorReason::Error
        },
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

impl StreamSink for ProxyStreamPusher {
    fn push(&mut self, event: AssistantMessageEvent) {
        self.push_inner(event)
    }
    fn end(&mut self, result: Option<AssistantMessage>) {
        self.end_inner(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
            extra: Default::default(),
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
        process_proxy_event(
            ProxyAssistantMessageEvent::TextStart { content_index: 0 },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        process_proxy_event(
            ProxyAssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hel".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        process_proxy_event(
            ProxyAssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "lo".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        assert!(
            matches!(&partial.content()[0], ContentBlock::Text { text, .. } if text == "hello")
        );
    }

    #[test]
    fn done_sets_reason_and_usage() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let ev = process_proxy_event(
            ProxyAssistantMessageEvent::Done {
                reason: ProxyDoneReason::Stop,
                usage: test_usage(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        assert!(matches!(
            ev,
            Some(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                ..
            })
        ));
        assert_eq!(partial.stop_reason(), Some(StopReason::Stop));
    }

    #[test]
    fn error_sets_error_message() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let ev = process_proxy_event(
            ProxyAssistantMessageEvent::Error {
                reason: ProxyErrorReason::Error,
                error_message: Some("boom".into()),
                usage: test_usage(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        assert!(matches!(
            ev,
            Some(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                ..
            })
        ));
        assert_eq!(partial.stop_reason(), Some(StopReason::Error));
        assert_eq!(partial.error_message(), Some("boom"));
    }

    #[test]
    fn tool_call_deltas_reconstruct_arguments() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallStart {
                content_index: 0,
                id: "tc1".into(),
                tool_name: "bash".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"command\": \"ls\"".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "}".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        assert!(
            matches!(&partial.content()[0], ContentBlock::ToolCall { name, .. } if name == "bash")
        );
        assert!(
            matches!(&partial.content()[0], ContentBlock::ToolCall { arguments, .. } if arguments.get("command").and_then(|v| v.as_str()) == Some("ls"))
        );
    }

    #[test]
    fn tool_call_end_replaces_block() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallStart {
                content_index: 0,
                id: "tc1".into(),
                tool_name: "bash".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"command\":\"ls\"}".into(),
            },
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        let _ = process_proxy_event(
            ProxyAssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ContentBlock::tool_call(
                    "tc1",
                    "bash",
                    serde_json::json!({"command": "ls -la"}),
                ),
            },
            &mut partial,
            &mut tool_partials,
        );
        assert!(matches!(&partial.content()[0], ContentBlock::ToolCall { id, .. } if id == "tc1"));
        assert!(
            matches!(&partial.content()[0], ContentBlock::ToolCall { arguments, .. } if arguments.get("command").and_then(|v| v.as_str()) == Some("ls -la"))
        );
        assert!(tool_partials.is_empty());
    }

    #[test]
    fn sse_line_parses_data_prefix() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = ProxyStreamPusher {
            tx,
            finished: false,
        };
        handle_sse_line(
            "data: {\"type\":\"start\"}",
            &mut sink,
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        handle_sse_line(
            "data: {\"type\":\"text_start\",\"contentIndex\":0}",
            &mut sink,
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        handle_sse_line(
            "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"hi\"}",
            &mut sink,
            &mut partial,
            &mut tool_partials,
        )
        .unwrap();
        assert!(matches!(&partial.content()[0], ContentBlock::Text { text, .. } if text == "hi"));
    }

    #[test]
    fn accepts_the_official_toolcall_wire_names() {
        let event: ProxyAssistantMessageEvent = serde_json::from_value(serde_json::json!({
            "type": "toolcall_start",
            "contentIndex": 0,
            "id": "call-1",
            "toolName": "bash"
        }))
        .expect("official proxy toolcall event should deserialize");
        assert!(matches!(
            event,
            ProxyAssistantMessageEvent::ToolCallStart {
                content_index: 0,
                id,
                tool_name
            } if id == "call-1" && tool_name == "bash"
        ));
    }

    #[test]
    fn accepts_camel_case_tool_use_stop_reason_and_rejects_invalid_usage() {
        let event: ProxyAssistantMessageEvent = serde_json::from_value(serde_json::json!({
            "type": "done",
            "reason": "toolUse",
            "usage": test_usage(),
        }))
        .expect("official toolUse stop reason should deserialize");
        assert!(matches!(
            event,
            ProxyAssistantMessageEvent::Done {
                reason: ProxyDoneReason::ToolUse,
                ..
            }
        ));

        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let error = process_proxy_event(
            ProxyAssistantMessageEvent::Done {
                reason: ProxyDoneReason::Stop,
                usage: serde_json::json!("not-usage"),
            },
            &mut partial,
            &mut tool_partials,
        )
        .expect_err("malformed usage must fail closed");
        assert!(error.contains("expected an object"), "{error}");
    }

    #[test]
    fn malformed_and_out_of_order_proxy_events_are_errors() {
        let mut partial = new_partial();
        let mut tool_partials = BTreeMap::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = ProxyStreamPusher {
            tx,
            finished: false,
        };
        let error = handle_sse_line(
            "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"x\"}",
            &mut sink,
            &mut partial,
            &mut tool_partials,
        )
        .expect_err("text deltas before text_start must fail closed");
        assert!(error.contains("text_delta"), "{error}");

        let error = handle_sse_line(
            "data: not-json",
            &mut sink,
            &mut partial,
            &mut tool_partials,
        )
        .expect_err("malformed SSE JSON must not be silently ignored");
        assert!(error.contains("Invalid proxy event"), "{error}");
    }

    #[test]
    fn proxy_utf8_decoder_preserves_split_code_points() {
        let mut carry = Vec::new();
        assert_eq!(decode_proxy_utf8_chunk(&mut carry, &[b'h', 0xc3]), "h");
        assert_eq!(decode_proxy_utf8_chunk(&mut carry, &[0xa9, b'!']), "é!");
        assert_eq!(finish_proxy_utf8(&mut carry, String::new()), "");
    }

    #[tokio::test]
    async fn an_already_aborted_proxy_request_returns_an_aborted_message() {
        let model = sample_model();
        let context = Context {
            system_prompt: None,
            messages: vec![],
            tools: Vec::<Tool>::new(),
        };
        let signal = Arc::new(AtomicBool::new(true));
        let stream = stream_proxy(
            &model,
            &context,
            ProxyStreamOptions {
                signal: Some(signal),
                auth_token: "token".into(),
                proxy_url: "http://127.0.0.1:1".into(),
                options: Default::default(),
            },
        );
        let (_, message) = stream.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
        assert_eq!(message.error_message(), Some("Request aborted by user"));
    }

    #[tokio::test]
    async fn stream_proxy_surfaces_error_for_unreachable_url() {
        let model = sample_model();
        let context = Context {
            system_prompt: None,
            messages: vec![],
            tools: Vec::<Tool>::new(),
        };
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
