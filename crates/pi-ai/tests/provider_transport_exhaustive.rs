#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Exhaustive loopback transport coverage for the registered provider surface.
//!
//! These tests deliberately use a real TCP listener and the public adaptor
//! entry points.  A pure parser fixture cannot prove URL construction,
//! headers, request serialization, response status handling, or cancellation
//! at the HTTP boundary, so every case below performs an actual reqwest call.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pi_ai::api::{
    anthropic_messages, azure_openai_responses, bedrock_converse, google_generative_ai,
    google_vertex, mistral_conversations, openai_codex_responses, openai_completions,
    openai_responses, pi_messages,
};
use pi_ai::model::Model;
use pi_ai::providers::all::builtin_providers;
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, DoneReason, Message,
    ProviderHeaders, ProviderRequestOptions, StopReason, StreamOptions, Tool, UserContent,
};

#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    delay: Duration,
}

impl Reply {
    fn text(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let count = socket.read(&mut chunk).await.expect("read request headers");
        assert!(count > 0, "client closed before sending request headers");
        raw.extend_from_slice(&chunk[..count]);
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .inspect(|(name, value)| {
            headers.insert(name.clone(), value.clone());
        })
        .find_map(|(name, value)| (name == "content-length").then(|| value.parse().unwrap_or(0)))
        .unwrap_or(0);

    while raw.len() < header_end + content_length {
        let count = socket.read(&mut chunk).await.expect("read request body");
        assert!(count > 0, "client closed before sending request body");
        raw.extend_from_slice(&chunk[..count]);
    }

    CapturedRequest {
        method,
        path,
        headers,
        body: raw[header_end..header_end + content_length].to_vec(),
    }
}

fn status_line(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        403 => "403 Forbidden",
        408 => "408 Request Timeout",
        429 => "429 Too Many Requests",
        500 => "500 Internal Server Error",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    }
}

async fn start_server(
    replies: Vec<Reply>,
) -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let address = listener.local_addr().expect("loopback listener address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_server = requests.clone();

    let task = tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.expect("accept loopback request");
            let request = read_request(&mut socket).await;
            requests_for_server.lock().unwrap().push(request);
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                status_line(reply.status),
                reply.content_type,
                reply.body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&reply.body).await;
        }
    });
    (format!("http://{address}"), requests, task)
}

fn sse(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        if event.as_str() == Some("[DONE]") {
            body.push_str("[DONE]");
        } else {
            body.push_str(&event.to_string());
        }
        body.push_str("\n\n");
    }
    body.into_bytes()
}

fn value(raw: &str) -> Value {
    serde_json::from_str(raw).expect("valid JSON test event")
}

fn named_sse(events: impl IntoIterator<Item = (&'static str, Value)>) -> Vec<u8> {
    let mut body = String::new();
    for (event, data) in events {
        body.push_str("event: ");
        body.push_str(event);
        body.push_str("\ndata: ");
        body.push_str(&data.to_string());
        body.push_str("\n\n");
    }
    body.into_bytes()
}

fn context_with_tool() -> Context {
    Context {
        system_prompt: Some("Be exact".to_string()),
        messages: vec![Message::User(UserContent::string("inspect this", 1))],
        tools: vec![Tool {
            name: "lookup".to_string(),
            description: "Look up a value".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"]
            }),
            constrained_sampling: None,
        }],
    }
}

fn request_options(key: &str) -> StreamOptions {
    let mut headers = ProviderHeaders::new();
    headers.insert("x-test-header".to_string(), Some("loopback".to_string()));
    StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some(key.to_string()),
            headers: Some(headers),
            max_retries: Some(0),
            ..Default::default()
        },
        temperature: Some(0.25),
        max_tokens: Some(32),
        session_id: Some("session-loopback".to_string()),
        ..Default::default()
    }
}

fn codex_token() -> String {
    let payload = base64::engine::general_purpose::STANDARD
        .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-loopback"}}"#);
    format!("aaa.{payload}.bbb")
}

fn representative_model(provider: &str, api: &str) -> Model {
    builtin_providers()
        .into_iter()
        .find(|candidate| {
            candidate.id == provider && candidate.models.iter().any(|model| model.api == api)
        })
        .and_then(|provider| provider.models.into_iter().find(|model| model.api == api))
        .unwrap_or_else(|| panic!("no catalog model for {provider}/{api}"))
}

fn local_model(provider: &str, api: &str, base_url: &str) -> Model {
    let mut model = if provider == "radius" && api == "pi-messages" {
        Model::new("auto", "Radius Auto", api, provider)
    } else {
        representative_model(provider, api)
    };
    model.base_url = base_url.to_string();
    model
}

fn simple_text_reply(api: &str) -> Reply {
    match api {
        "openai-completions" => Reply::text(
            200,
            "text/event-stream",
            sse([
                json!({"id":"completion-loopback","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}),
                json!({"id":"completion-loopback","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}),
                json!("[DONE]"),
            ]),
        ),
        "openai-responses" | "azure-openai-responses" => Reply::text(
            200,
            "text/event-stream",
            sse([
                json!({"type":"response.created","response":{"id":"response-loopback"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"message-loopback","role":"assistant","status":"in_progress","content":[]}}),
                json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"message-loopback","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello"}]}}),
                json!({"type":"response.completed","response":{"id":"response-loopback","status":"completed","usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}),
            ]),
        ),
        "anthropic-messages" => Reply::text(
            200,
            "text/event-stream",
            named_sse([
                (
                    "message_start",
                    json!({"type":"message_start","message":{"id":"anthropic-loopback","model":"loopback","usage":{"input_tokens":7,"output_tokens":0}}}),
                ),
                (
                    "content_block_start",
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                ),
                (
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
                ),
                (
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":0}),
                ),
                (
                    "message_delta",
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                ),
                ("message_stop", json!({"type":"message_stop"})),
            ]),
        ),
        "google-generative-ai" | "google-vertex" => Reply::text(
            200,
            "text/event-stream",
            sse([json!({
                "responseId":"google-loopback",
                "candidates":[{"content":{"parts":[{"text":"hello"}],"role":"model"},"finishReason":"STOP"}],
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"totalTokenCount":9}
            })]),
        ),
        "mistral-conversations" => Reply::text(
            200,
            "text/event-stream",
            sse([
                json!({"id":"mistral-loopback","model":"loopback","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}),
                json!({"id":"mistral-loopback","model":"loopback","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}),
                json!("[DONE]"),
            ]),
        ),
        "openai-codex-responses" => Reply::text(
            200,
            "text/event-stream",
            sse([
                json!({"type":"response.created","response":{"id":"codex-loopback","status":"in_progress"}}),
                json!({"type":"response.output_item.added","item":{"type":"message","id":"codex-message","role":"assistant","status":"in_progress","content":[]}}),
                json!({"type":"response.output_text.delta","delta":"hello"}),
                json!({"type":"response.output_item.done","item":{"type":"message","id":"codex-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello"}]}}),
                json!({"type":"response.completed","response":{"id":"codex-loopback","status":"completed","usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}),
            ]),
        ),
        "bedrock-converse-stream" => Reply::text(
            200,
            "application/vnd.amazon.eventstream",
            bedrock_frames([
                ("messageStart", json!({"messageStart":{"role":"assistant"}})),
                (
                    "contentBlockStart",
                    json!({"contentBlockStart":{"contentBlockIndex":0,"start":{}}}),
                ),
                (
                    "contentBlockDelta",
                    json!({"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hello"}}}),
                ),
                (
                    "contentBlockStop",
                    json!({"contentBlockStop":{"contentBlockIndex":0}}),
                ),
                (
                    "metadata",
                    json!({"metadata":{"usage":{"inputTokens":7,"outputTokens":2,"totalTokens":9}}}),
                ),
                (
                    "messageStop",
                    json!({"messageStop":{"stopReason":"end_turn"}}),
                ),
            ]),
        ),
        "pi-messages" => Reply::text(
            200,
            "text/event-stream",
            sse([
                json!({"type":"start"}),
                json!({"type":"text_start","contentIndex":0}),
                json!({"type":"text_delta","contentIndex":0,"delta":"hello"}),
                json!({"type":"text_end","contentIndex":0,"content":"hello"}),
                json!({"type":"done","reason":"stop","responseId":"pi-loopback","usage":{"input":7,"output":2,"totalTokens":9,"cacheRead":0,"cacheWrite":0}}),
            ]),
        ),
        other => panic!("no loopback reply for {other}"),
    }
}

fn collect_text(message: &AssistantMessage) -> String {
    message
        .content()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_success(message: &AssistantMessage, expected_api: &str, expected_text: &str) {
    assert_eq!(message.api(), Some(expected_api));
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(collect_text(message), expected_text);
    assert_eq!(message.usage().map(|usage| usage.input), Some(7));
    assert_eq!(message.usage().map(|usage| usage.output), Some(2));
}

fn assert_auth_and_common_headers(request: &CapturedRequest, key: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.headers.get("authorization"),
        Some(&format!("Bearer {key}"))
    );
    assert_eq!(
        request.headers.get("x-test-header"),
        Some(&"loopback".to_string())
    );
    assert!(!String::from_utf8_lossy(&request.body).contains("x-api-key-secret"));
}

#[cfg(target_os = "linux")]
#[link(name = "zstd")]
unsafe extern "C" {
    fn ZSTD_getFrameContentSize(src: *const u8, src_size: usize) -> u64;
    fn ZSTD_decompress(
        dst: *mut u8,
        dst_capacity: usize,
        src: *const u8,
        compressed_size: usize,
    ) -> usize;
    fn ZSTD_isError(code: usize) -> u32;
}

fn request_json_body(request: &CapturedRequest) -> Value {
    let body = if request.headers.get("content-encoding").map(String::as_str) == Some("zstd") {
        #[cfg(target_os = "linux")]
        {
            // Codex uses the same zstd request-body optimization as the
            // production adapter. Decode the bytes at the loopback boundary
            // before asserting the wire JSON rather than weakening the test.
            const CONTENT_SIZE_ERROR: u64 = u64::MAX;
            const CONTENT_SIZE_UNKNOWN: u64 = u64::MAX - 1;
            let size =
                unsafe { ZSTD_getFrameContentSize(request.body.as_ptr(), request.body.len()) };
            assert!(
                size != CONTENT_SIZE_ERROR && size != CONTENT_SIZE_UNKNOWN,
                "zstd request must declare a bounded content size"
            );
            let mut decoded = vec![0_u8; size as usize];
            let written = unsafe {
                ZSTD_decompress(
                    decoded.as_mut_ptr(),
                    decoded.len(),
                    request.body.as_ptr(),
                    request.body.len(),
                )
            };
            assert_eq!(
                unsafe { ZSTD_isError(written) },
                0,
                "zstd request decode failed"
            );
            decoded.truncate(written);
            decoded
        }
        #[cfg(not(target_os = "linux"))]
        {
            panic!("zstd Codex request decoding is not available on this target");
        }
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).unwrap_or_else(|error| {
        panic!(
            "request body is not JSON ({error}); path={} content-encoding={:?} body={:?}",
            request.path,
            request.headers.get("content-encoding"),
            String::from_utf8_lossy(&body)
        )
    })
}

async fn run_direct_stream(
    api: &str,
    provider: &str,
    reply: Reply,
    options: StreamOptions,
    context: Context,
) -> (
    Vec<AssistantMessageEvent>,
    AssistantMessage,
    CapturedRequest,
) {
    let (base_url, requests, server) = start_server(vec![reply]).await;
    let model = local_model(provider, api, &base_url);
    let key = options.base.api_key.clone().unwrap_or_default();
    let stream = match api {
        "openai-completions" => openai_completions::stream(
            &model,
            &context,
            reqwest::Client::new(),
            &base_url,
            Some(&key),
            &openai_completions::OpenAIChatOptions {
                base: options,
                ..Default::default()
            },
        ),
        "openai-responses" => openai_responses::stream(
            &model,
            &context,
            reqwest::Client::new(),
            &base_url,
            Some(&key),
            &openai_responses::OpenAIResponsesOptions::from_stream_options(options),
        ),
        "azure-openai-responses" => azure_openai_responses::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some(&key),
            &azure_openai_responses::AzureOpenAIResponsesOptions {
                base: options,
                ..Default::default()
            },
        ),
        "anthropic-messages" => anthropic_messages::stream(
            &model,
            &context,
            reqwest::Client::new(),
            &base_url,
            Some(&key),
            &anthropic_messages::AnthropicOptions {
                base: options,
                ..Default::default()
            },
        ),
        "google-generative-ai" => google_generative_ai::stream(
            &model,
            &context,
            reqwest::Client::new(),
            &base_url,
            Some(&key),
            &google_generative_ai::GoogleOptions::from_stream_options(options),
        ),
        "google-vertex" => google_vertex::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some(&key),
            &google_vertex::GoogleVertexOptions {
                base: options,
                project: Some("loopback-project".to_string()),
                location: Some("loopback-location".to_string()),
                ..Default::default()
            },
        ),
        "mistral-conversations" => mistral_conversations::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some(&key),
            &mistral_conversations::MistralOptions {
                base: options,
                ..Default::default()
            },
        ),
        "openai-codex-responses" => openai_codex_responses::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some(&key),
            &openai_codex_responses::OpenAICodexResponsesOptions {
                base: {
                    let mut options = options;
                    options.transport = Some("sse".to_string());
                    options
                },
                transport: Some("sse".to_string()),
                ..Default::default()
            },
        ),
        "bedrock-converse-stream" => {
            let mut options = options;
            options.base.env = Some(BTreeMap::from([
                ("AWS_REGION".to_string(), "us-east-1".to_string()),
                ("AWS_BEDROCK_SKIP_AUTH".to_string(), "1".to_string()),
            ]));
            bedrock_converse::stream(
                &model,
                &context,
                reqwest::Client::new(),
                Some(&key),
                &bedrock_converse::BedrockOptions {
                    base: options,
                    ..Default::default()
                },
            )
        }
        "pi-messages" => pi_messages::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some(&key),
            &pi_messages::PiMessagesOptions {
                base: options,
                ..Default::default()
            },
        ),
        other => panic!("unsupported direct api {other}"),
    };
    let (events, message) = stream.collect().await;
    server.await.expect("loopback server task");
    let request = requests
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("adaptor made an HTTP request");
    (events, message, request)
}

fn bedrock_header(name: &str, value: &str) -> Vec<u8> {
    let mut bytes = vec![name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(6); // string header value
    bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    bytes
}

fn bedrock_frames<'a>(events: impl IntoIterator<Item = (&'a str, Value)>) -> Vec<u8> {
    let mut output = Vec::new();
    for (event_type, payload) in events {
        let headers = [
            bedrock_header(":message-type", "event"),
            bedrock_header(":event-type", event_type),
        ]
        .concat();
        let payload = payload.to_string().into_bytes();
        let total_length = 16 + headers.len() + payload.len() + 4;
        let headers_length = headers.len();
        let mut frame = Vec::with_capacity(total_length);
        frame.extend_from_slice(&[0x00, 0xc0, 0xde, 0x00]);
        frame.extend_from_slice(&(total_length as u32).to_be_bytes());
        frame.extend_from_slice(&(headers_length as u32).to_be_bytes());
        let prelude_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&payload);
        let message_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());
        output.extend_from_slice(&frame);
    }
    output
}

fn tool_call_count(message: &AssistantMessage) -> usize {
    message
        .content()
        .iter()
        .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
        .count()
}

#[tokio::test]
async fn every_registered_provider_api_pair_uses_a_real_loopback_request() {
    let mut pairs = BTreeSet::new();
    for provider in builtin_providers() {
        for model in provider.models {
            pairs.insert((model.provider, model.api));
        }
    }

    assert_eq!(pairs.len(), 49, "registered catalog pair count changed");
    assert!(!pairs.iter().any(|(_, api)| api == "unknown-api"));
    assert!(pairs
        .iter()
        .any(|(provider, api)| provider == "openai" && api == "openai-responses"));

    for (provider, api) in pairs {
        if api == "pi-messages" {
            continue;
        }
        let key = if api == "openai-codex-responses" {
            codex_token()
        } else {
            "x-api-key-secret".to_string()
        };
        let (events, message, request) = run_direct_stream(
            &api,
            &provider,
            simple_text_reply(&api),
            request_options(&key),
            Context {
                messages: vec![Message::User(UserContent::string("hello", 1))],
                ..Default::default()
            },
        )
        .await;
        assert_success(&message, &api, "hello");
        assert!(events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Done { .. })));
        if api == "bedrock-converse-stream" {
            assert!(request.headers.contains_key("authorization"));
        } else if api == "google-generative-ai" || api == "google-vertex" {
            assert_eq!(
                request.headers.get("x-goog-api-key"),
                Some(&key.to_string())
            );
        } else if api == "anthropic-messages" {
            assert!(
                request.headers.get("x-api-key") == Some(&key.to_string())
                    || request.headers.get("authorization") == Some(&format!("Bearer {key}")),
                "Anthropic auth header missing: {:?}",
                request.headers
            );
        } else {
            assert_auth_and_common_headers(&request, &key);
        }
        let body = request_json_body(&request);
        match api.as_str() {
            "openai-completions" => {
                assert!(request.path.ends_with("/chat/completions"));
                assert_eq!(body["model"], representative_model(&provider, &api).id);
                assert!(body["messages"].is_array());
                assert_eq!(body["stream"], true);
            }
            "openai-responses" | "azure-openai-responses" | "openai-codex-responses" => {
                assert!(request.path.contains("/responses"));
                assert!(body["input"].is_array());
                assert_eq!(body["stream"], true);
            }
            "anthropic-messages" => {
                assert!(request.path.ends_with("/v1/messages"));
                assert!(body["messages"].is_array());
                assert_eq!(body["stream"], true);
            }
            "google-generative-ai" | "google-vertex" => {
                assert!(request.path.contains("streamGenerateContent"));
                assert!(body["contents"].is_array());
            }
            "mistral-conversations" => {
                assert!(request.path.ends_with("/v1/chat/completions"));
                assert!(body["messages"].is_array());
                assert_eq!(body["stream"], true);
            }
            "bedrock-converse-stream" => {
                assert!(request.path.contains(":converse-stream"));
                assert!(body["messages"].is_array());
                assert!(body["modelId"].is_string());
            }
            other => panic!("unhandled registered API {other}"),
        }
    }
}

#[tokio::test]
async fn public_provider_runtime_dispatches_loopback_cloudflare_and_native_adaptors() {
    for (provider_id, api) in [
        ("cloudflare-ai-gateway", "openai-completions"),
        ("cloudflare-ai-gateway", "openai-responses"),
        ("cloudflare-ai-gateway", "anthropic-messages"),
        ("cloudflare-workers-ai", "openai-completions"),
        ("mistral", "mistral-conversations"),
        ("openai-codex", "openai-codex-responses"),
        ("google-vertex", "google-vertex"),
    ] {
        let (base_url, requests, server) = start_server(vec![simple_text_reply(api)]).await;
        let provider = builtin_providers()
            .into_iter()
            .find(|candidate| candidate.id == provider_id)
            .unwrap_or_else(|| panic!("missing provider {provider_id}"));
        let mut model = provider
            .models
            .clone()
            .into_iter()
            .find(|model| model.api == api)
            .unwrap_or_else(|| panic!("missing {provider_id}/{api} model"));
        model.base_url = base_url;
        let key = if api == "openai-codex-responses" {
            codex_token()
        } else {
            "runtime-loopback-key".to_string()
        };
        let mut options = request_options(&key);
        if api == "openai-codex-responses" {
            options.transport = Some("sse".to_string());
        }
        if api == "google-vertex" {
            options.base.env = Some(BTreeMap::from([
                (
                    "GOOGLE_CLOUD_PROJECT".to_string(),
                    "loopback-project".to_string(),
                ),
                (
                    "GOOGLE_CLOUD_LOCATION".to_string(),
                    "loopback-location".to_string(),
                ),
            ]));
        }
        let (events, message) = provider
            .stream(
                &model,
                &Context {
                    messages: vec![Message::User(UserContent::string("runtime", 1))],
                    ..Default::default()
                },
                Some(&options),
            )
            .collect()
            .await;
        server.await.expect("provider loopback server task");
        assert_eq!(
            message.stop_reason(),
            Some(StopReason::Stop),
            "{provider_id}/{api}: {:?}",
            message.error_message()
        );
        assert_eq!(message.api(), Some(api));
        assert_eq!(collect_text(&message), "hello", "{provider_id}/{api}");
        assert_eq!(message.usage().map(|usage| usage.input), Some(7));
        assert_eq!(message.usage().map(|usage| usage.output), Some(2));
        assert!(events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Done { .. })));
        assert_eq!(requests.lock().unwrap().len(), 1, "{provider_id}/{api}");
    }
}

#[tokio::test]
async fn rich_stream_translation_covers_text_thinking_tool_usage_and_done() {
    let rich_completion = sse([
        json!({"id":"rich-completion","choices":[{"index":0,"delta":{"reasoning_content":"consider"},"finish_reason":null}]}),
        json!({"id":"rich-completion","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
        value(
            r#"{"id":"rich-completion","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-rich","type":"function","function":{"name":"lookup","arguments":"{\"key\":"}}]},"finish_reason":null}]}"#,
        ),
        value(
            r#"{"id":"rich-completion","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"value\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":4,"total_tokens":11}}"#,
        ),
        json!("[DONE]"),
    ]);
    let (events, message, request) = run_direct_stream(
        "openai-completions",
        "deepseek",
        Reply::text(200, "text/event-stream", rich_completion),
        request_options("rich-key"),
        context_with_tool(),
    )
    .await;
    assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
    assert_eq!(collect_text(&message), "answer");
    assert_eq!(tool_call_count(&message), 1);
    assert!(message.content().iter().any(
        |block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "consider")
    ));
    assert_eq!(message.usage().map(|usage| usage.total_tokens), Some(11));
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            ..
        }
    )));
    assert_auth_and_common_headers(&request, "rich-key");

    let rich_responses = sse([
        json!({"type":"response.created","response":{"id":"rich-response"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"reasoning-1","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"reasoning-1","summary_index":0,"delta":"consider"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"reasoning-1","summary":[{"type":"summary_text","text":"consider"}]}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"function-1","call_id":"call-response","name":"lookup","arguments":""}}),
        value(
            r#"{"type":"response.function_call_arguments.delta","item_id":"function-1","output_index":1,"delta":"{\"key\":\"value\"}"}"#,
        ),
        value(
            r#"{"type":"response.function_call_arguments.done","item_id":"function-1","output_index":1,"arguments":"{\"key\":\"value\"}"}"#,
        ),
        json!({"type":"response.completed","response":{"id":"rich-response","status":"completed","usage":{"input_tokens":7,"output_tokens":4,"total_tokens":11}}}),
    ]);
    let (events, message, _) = run_direct_stream(
        "openai-responses",
        "openai",
        Reply::text(200, "text/event-stream", rich_responses),
        request_options("rich-key"),
        context_with_tool(),
    )
    .await;
    assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
    assert_eq!(tool_call_count(&message), 1);
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::ToolCallDelta { .. })));
    assert_eq!(message.usage().map(|usage| usage.total_tokens), Some(11));
}

#[tokio::test]
async fn provider_specific_headers_and_request_shapes_are_observable() {
    let (events, message, request) = run_direct_stream(
        "anthropic-messages",
        "anthropic",
        Reply::text(
            200,
            "text/event-stream",
            simple_text_reply("anthropic-messages").body,
        ),
        request_options("anthropic-key"),
        context_with_tool(),
    )
    .await;
    assert_success(&message, "anthropic-messages", "hello");
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::Done { .. })));
    assert_eq!(
        request.headers.get("x-api-key"),
        Some(&"anthropic-key".to_string())
    );
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(
        request.headers.get("anthropic-version"),
        Some(&"2023-06-01".to_string())
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["system"][0]["text"], "Be exact");
    assert_eq!(body["tools"][0]["name"], "lookup");

    let (events, message, request) = run_direct_stream(
        "google-generative-ai",
        "google",
        simple_text_reply("google-generative-ai"),
        request_options("google-key"),
        context_with_tool(),
    )
    .await;
    assert_success(&message, "google-generative-ai", "hello");
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::TextDelta { .. })));
    assert_eq!(
        request.headers.get("x-goog-api-key"),
        Some(&"google-key".to_string())
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body["contents"].is_array());
    assert!(body["tools"].is_array());
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 32);

    let (_, message, request) = run_direct_stream(
        "openai-codex-responses",
        "openai-codex",
        simple_text_reply("openai-codex-responses"),
        request_options(&codex_token()),
        Context::default(),
    )
    .await;
    assert_success(&message, "openai-codex-responses", "hello");
    assert_eq!(
        request.headers.get("chatgpt-account-id"),
        Some(&"acct-loopback".to_string())
    );
    assert_eq!(request.headers.get("originator"), Some(&"pi".to_string()));
    assert_eq!(
        request.headers.get("openai-beta"),
        Some(&"responses=experimental".to_string())
    );
}

#[tokio::test]
async fn malformed_empty_and_http_error_responses_become_terminal_errors_without_key_leakage() {
    for (api, provider, body, content_type) in [
        (
            "openai-completions",
            "deepseek",
            sse([json!({"choices":[]}), json!("[DONE]")]),
            "text/event-stream",
        ),
        (
            "openai-completions",
            "moonshotai",
            sse([json!({"choices":[]}), json!("[DONE]")]),
            "text/event-stream",
        ),
        (
            "openai-completions",
            "moonshotai-cn",
            sse([json!({"choices":[]}), json!("[DONE]")]),
            "text/event-stream",
        ),
        (
            "openai-completions",
            "nvidia",
            sse([json!({"choices":[]}), json!("[DONE]")]),
            "text/event-stream",
        ),
        (
            "openai-responses",
            "openai",
            b"".to_vec(),
            "text/event-stream",
        ),
        (
            "anthropic-messages",
            "anthropic",
            b"event: message_start\ndata: {broken}\n\n".to_vec(),
            "text/event-stream",
        ),
    ] {
        let (events, message, _) = run_direct_stream(
            api,
            provider,
            Reply::text(200, content_type, body),
            request_options("never-echo-this-secret"),
            Context::default(),
        )
        .await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error), "{api}");
        assert!(events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Error { .. })));
        assert!(!message
            .error_message()
            .unwrap_or("")
            .contains("never-echo-this-secret"));
    }

    let (base_url, requests, server) = start_server(vec![Reply::text(
        429,
        "application/json",
        br#"{"error":{"message":"quota exceeded"}}"#.to_vec(),
    )])
    .await;
    let mut model = local_model("openai", "openai-responses", &base_url);
    let key = "http-error-secret";
    let message = openai_responses::stream(
        &model,
        &Context::default(),
        reqwest::Client::new(),
        &base_url,
        Some(key),
        &openai_responses::OpenAIResponsesOptions::from_stream_options(request_options(key)),
    )
    .collect()
    .await
    .1;
    server.await.unwrap();
    model.base_url = base_url;
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert!(message
        .error_message()
        .unwrap_or("")
        .contains("quota exceeded"));
    assert!(!message.error_message().unwrap_or("").contains(key));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn every_http_adaptor_translates_a_non_success_response_to_a_redacted_error() {
    for (provider, api) in [
        ("deepseek", "openai-completions"),
        ("openai", "openai-responses"),
        ("azure-openai-responses", "azure-openai-responses"),
        ("anthropic", "anthropic-messages"),
        ("google", "google-generative-ai"),
        ("google-vertex", "google-vertex"),
        ("mistral", "mistral-conversations"),
        ("openai-codex", "openai-codex-responses"),
        ("amazon-bedrock", "bedrock-converse-stream"),
        ("radius", "pi-messages"),
        ("moonshotai", "openai-completions"),
        ("moonshotai-cn", "openai-completions"),
        ("nvidia", "openai-completions"),
    ] {
        let key = if api == "openai-codex-responses" {
            codex_token()
        } else {
            "http-error-secret".to_string()
        };
        let body = if api == "pi-messages" {
            br#"{"error":{"message":"transport denied","code":"denied"}}"#.to_vec()
        } else {
            br#"{"error":{"message":"transport denied"}}"#.to_vec()
        };
        let (events, message, _) = run_direct_stream(
            api,
            provider,
            Reply::text(503, "application/json", body),
            request_options(&key),
            Context::default(),
        )
        .await;
        assert_eq!(
            message.stop_reason(),
            Some(StopReason::Error),
            "{provider}/{api}: {:?}",
            message.error_message()
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Error { .. })));
        assert!(
            message
                .error_message()
                .unwrap_or("")
                .contains("transport denied"),
            "{provider}/{api}: {:?}",
            message.error_message()
        );
        assert!(!message.error_message().unwrap_or("").contains(&key));
    }
}

#[tokio::test]
async fn retry_timeout_and_abort_paths_are_exercised_where_supported() {
    let mut attempts = 0_u32;
    let policy = pi_ai::utils::RetryPolicy {
        enabled: true,
        max_retries: 1,
        base_delay_ms: 1,
    };
    let message = pi_ai::utils::retry_assistant_call(
        || {
            attempts += 1;
            let reply = if attempts == 1 {
                Reply::text(
                    503,
                    "application/json",
                    br#"{"error":{"message":"service overloaded"}}"#.to_vec(),
                )
            } else {
                simple_text_reply("openai-responses")
            };
            async move {
                run_direct_stream(
                    "openai-responses",
                    "openai",
                    reply,
                    request_options("retry-key"),
                    Context::default(),
                )
                .await
                .1
            }
        },
        Some(&policy),
        None,
        None,
    )
    .await;
    assert_success(&message, "openai-responses", "hello");
    assert_eq!(attempts, 2);

    let (base_url, _, server) = start_server(vec![
        simple_text_reply("mistral-conversations").delayed(Duration::from_millis(100))
    ])
    .await;
    let mut options = request_options("timeout-key");
    options.base.timeout_ms = Some(10);
    let model = local_model("mistral", "mistral-conversations", &base_url);
    let message = mistral_conversations::stream(
        &model,
        &Context::default(),
        reqwest::Client::new(),
        Some("timeout-key"),
        &mistral_conversations::MistralOptions {
            base: options,
            ..Default::default()
        },
    )
    .collect()
    .await
    .1;
    server.await.unwrap();
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert!(message
        .error_message()
        .unwrap_or("")
        .to_ascii_lowercase()
        .contains("timed out"));

    let (base_url, _, server) = start_server(vec![Reply::text(
        200,
        "application/json",
        br#"{"baseUrl":"http://loopback/v1","models":[]}"#.to_vec(),
    )
    .delayed(Duration::from_millis(200))])
    .await;
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let request_base_url = base_url.clone();
    let cancel = cancelled.clone();
    let cancel_for_task = cancel.clone();
    let task = tokio::spawn(async move {
        pi_ai::providers::load_radius_gateway_config(
            &request_base_url,
            Some("radius-key"),
            Some(cancel_for_task),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let result = task.await.unwrap();
    server.await.unwrap();
    assert_eq!(result.unwrap_err(), "Request cancelled");
}

#[tokio::test]
async fn moonshot_and_nvidia_loopback_retry_and_abort_are_deterministic() {
    for provider in ["moonshotai", "moonshotai-cn", "nvidia"] {
        let (base_url, requests, server) = start_server(vec![
            Reply::text(
                503,
                "application/json",
                br#"{"error":{"message":"temporary overload"}}"#.to_vec(),
            ),
            simple_text_reply("openai-completions"),
        ])
        .await;
        let model = local_model(provider, "openai-completions", &base_url);
        let key = "provider-retry-key";
        let mut options = request_options(key);
        options.base.max_retries = Some(1);
        let (_, message) = openai_completions::stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            &base_url,
            Some(key),
            &openai_completions::OpenAIChatOptions {
                base: options,
                ..Default::default()
            },
        )
        .collect()
        .await;
        server.await.expect("retry loopback server task");
        assert_success(&message, "openai-completions", "hello");
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "{provider}");
            assert_eq!(requests[0].path, "/chat/completions", "{provider}");
            assert_eq!(
                requests[0].headers.get("authorization"),
                Some(&format!("Bearer {key}")),
                "{provider}"
            );
            let body = request_json_body(&requests[0]);
            assert_eq!(body["model"], model.id, "{provider}");
            assert_eq!(body["max_tokens"], 32, "{provider}");
            if provider == "nvidia" {
                assert_eq!(
                    requests[0].headers.get("nvcf-poll-seconds"),
                    Some(&"3600".to_string())
                );
            }
        }

        let (base_url, _, server) = start_server(vec![
            simple_text_reply("openai-completions").delayed(Duration::from_millis(200))
        ])
        .await;
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut options = request_options("provider-abort-key");
        options.abort_signal = Some(signal.clone());
        let model = local_model(provider, "openai-completions", &base_url);
        let task = tokio::spawn(async move {
            openai_completions::stream(
                &model,
                &Context::default(),
                reqwest::Client::new(),
                &base_url,
                Some("provider-abort-key"),
                &openai_completions::OpenAIChatOptions {
                    base: options,
                    ..Default::default()
                },
            )
            .collect()
            .await
            .1
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        signal.store(true, std::sync::atomic::Ordering::SeqCst);
        let message = task.await.expect("abort stream task");
        server.await.expect("abort loopback server task");
        assert_eq!(
            message.stop_reason(),
            Some(StopReason::Aborted),
            "{provider}"
        );
        assert!(!message
            .error_message()
            .unwrap_or("")
            .contains("provider-abort-key"));
    }
}

#[tokio::test]
async fn radius_pi_messages_transport_carries_wire_events_and_options() {
    let (base_url, requests, server) = start_server(vec![simple_text_reply("pi-messages")]).await;
    let mut model = Model::new("auto", "Radius Auto", "pi-messages", "radius");
    model.base_url = base_url.clone();
    let mut options = request_options("radius-key");
    options.cache_retention = Some("long".to_string());
    let stream = pi_messages::stream(
        &model,
        &context_with_tool(),
        reqwest::Client::new(),
        Some("radius-key"),
        &pi_messages::PiMessagesOptions {
            base: options,
            reasoning: Some("high".to_string()),
            tool_choice: Some(json!("auto")),
            debug: true,
        },
    );
    let (events, message) = stream.collect().await;
    server.await.unwrap();
    assert_success(&message, "pi-messages", "hello");
    assert_eq!(message.response_id(), Some("pi-loopback"));
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::TextDelta { .. })));
    let request = requests.lock().unwrap().first().cloned().unwrap();
    assert_eq!(request.path, "/messages?debug=1");
    assert_auth_and_common_headers(&request, "radius-key");
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "auto");
    assert_eq!(body["options"]["maxTokens"], 32);
    assert_eq!(body["options"]["reasoning"], "high");
    assert_eq!(body["options"]["toolChoice"], "auto");
    assert_eq!(body["options"]["cacheRetention"], "long");
}

#[tokio::test]
async fn bedrock_binary_event_stream_translates_tool_thinking_usage_and_done() {
    let body = bedrock_frames([
        ("messageStart", json!({"messageStart":{"role":"assistant"}})),
        (
            "contentBlockStart",
            json!({"contentBlockStart":{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tool-1","name":"lookup"}}}}),
        ),
        (
            "contentBlockDelta",
            value(
                r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"key\":\"value\"}"}}}}"#,
            ),
        ),
        (
            "contentBlockStop",
            json!({"contentBlockStop":{"contentBlockIndex":0}}),
        ),
        (
            "contentBlockStart",
            json!({"contentBlockStart":{"contentBlockIndex":1,"start":{"reasoningContent":{}}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockDelta":{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"consider"}}}}),
        ),
        (
            "contentBlockStop",
            json!({"contentBlockStop":{"contentBlockIndex":1}}),
        ),
        (
            "metadata",
            json!({"metadata":{"usage":{"inputTokens":7,"outputTokens":4,"totalTokens":11}}}),
        ),
        (
            "messageStop",
            json!({"messageStop":{"stopReason":"tool_use"}}),
        ),
    ]);
    let (events, message, request) = run_direct_stream(
        "bedrock-converse-stream",
        "amazon-bedrock",
        Reply::text(200, "application/vnd.amazon.eventstream", body),
        request_options("bedrock-key"),
        context_with_tool(),
    )
    .await;
    assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
    assert_eq!(tool_call_count(&message), 1);
    assert!(message.content().iter().any(
        |block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "consider")
    ));
    assert_eq!(message.usage().map(|usage| usage.total_tokens), Some(11));
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            ..
        }
    )));
    assert!(request.path.contains(":converse-stream"));
    assert!(request.headers.contains_key("authorization"));
}

#[tokio::test]
async fn images_transport_is_real_http_and_redacts_auth_from_errors() {
    let (base_url, requests, server) = start_server(vec![Reply::text(
        200,
        "application/json",
        br#"{"id":"image-loopback","choices":[{"message":{"content":[{"type":"text","text":"done"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}"#.to_vec(),
    )])
    .await;
    let mut model = pi_ai::images::catalog_images("openrouter")
        .into_iter()
        .next()
        .expect("openrouter image model");
    model.base_url = base_url;
    let result = pi_ai::api::openrouter_images::generate_images(
        &model,
        &pi_ai::types::ImagesContext {
            input: vec![ContentBlock::text("draw a loopback")],
        },
        &pi_ai::images::ImagesOptions {
            api_key: Some("image-secret".to_string()),
            max_retries: Some(0),
            ..Default::default()
        },
        reqwest::Client::new(),
    )
    .await;
    server.await.unwrap();
    assert_eq!(result.stop_reason, pi_ai::types::ImagesStopReason::Stop);
    assert_eq!(result.response_id.as_deref(), Some("image-loopback"));
    assert_eq!(result.usage.as_ref().map(|usage| usage.input), Some(7));
    let request = requests.lock().unwrap().first().cloned().unwrap();
    assert!(request.path.ends_with("/chat/completions"));
    assert_eq!(
        request.headers.get("authorization"),
        Some(&"Bearer image-secret".to_string())
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["stream"], false);
    assert_eq!(body["modalities"], json!(["image"]));

    let (base_url, requests, server) = start_server(vec![Reply::text(
        500,
        "application/json",
        br#"{"error":{"message":"image denied"}}"#.to_vec(),
    )])
    .await;
    model.base_url = base_url;
    let result = pi_ai::api::openrouter_images::generate_images(
        &model,
        &pi_ai::types::ImagesContext {
            input: vec![ContentBlock::text("draw a loopback")],
        },
        &pi_ai::images::ImagesOptions {
            api_key: Some("image-secret".to_string()),
            max_retries: Some(0),
            ..Default::default()
        },
        reqwest::Client::new(),
    )
    .await;
    server.await.unwrap();
    assert_eq!(result.stop_reason, pi_ai::types::ImagesStopReason::Error);
    assert!(result
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("image denied"));
    assert!(!result
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("image-secret"));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn registered_provider_inventory_is_explicit_about_non_http_surfaces() {
    let providers = builtin_providers();
    assert_eq!(providers.len(), 40);
    let ids: BTreeSet<_> = providers
        .iter()
        .map(|provider| provider.id.as_str())
        .collect();
    assert!(ids.contains("radius"));
    assert!(providers
        .iter()
        .any(|provider| provider.id == "radius" && provider.models.is_empty()));
    assert!(pi_ai::types::KNOWN_PROVIDERS.contains(&"faux"));
    // faux is deliberately scripted and Radius receives its catalog over
    // /v1/config; neither has a bundled catalog pair to put in the matrix.
}
