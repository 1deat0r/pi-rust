#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real loopback transport coverage for the non-Codex, non-Bedrock,
//! non-pi-messages adaptors in the abort/payload parity slice.
//!
//! The server below is an actual TCP HTTP/1.1 peer. It records the request
//! body, returns provider-shaped SSE, and can deliberately leave a chunked
//! response open so the adaptor has to cancel its in-flight body read.

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use pi_ai::api::{
    anthropic_messages::{self, AnthropicOptions},
    azure_openai_responses::{self, AzureOpenAIResponsesOptions},
    google_generative_ai::{self, GoogleOptions},
    google_vertex::{self, GoogleVertexOptions},
    mistral_conversations::{self, MistralOptions},
    openai_completions::{self, OpenAIChatOptions},
    openai_responses::{self, OpenAIResponsesOptions},
};
use pi_ai::event_stream::AssistantMessageEventStream;
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, Context, ErrorReason, Message, OnPayloadFn,
    OnPayloadFuture, ProviderRequestOptions, StopReason, StreamOptions, UserContent,
};
use pi_ai::Model;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex, Notify};

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    body: String,
}

struct LoopbackServer {
    base_url: String,
    captured: Arc<Mutex<Option<CapturedRequest>>>,
    request_received: Arc<Notify>,
    response_started: Arc<Notify>,
    shutdown: Option<oneshot::Sender<()>>,
    release_body: Option<oneshot::Sender<()>>,
}

fn spawn_loopback(response_body: &str, hold_body_open: bool) -> LoopbackServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    listener
        .set_nonblocking(true)
        .expect("set loopback listener nonblocking");
    let address = listener.local_addr().expect("loopback address");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio loopback listener");

    let captured = Arc::new(Mutex::new(None));
    let request_received = Arc::new(Notify::new());
    let response_started = Arc::new(Notify::new());
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (release_tx, mut release_rx) = oneshot::channel();
    let response_body = response_body.to_string();

    let captured_for_task = Arc::clone(&captured);
    let request_received_for_task = Arc::clone(&request_received);
    let response_started_for_task = Arc::clone(&response_started);
    tokio::spawn(async move {
        let accepted = tokio::select! {
            _ = &mut shutdown_rx => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((mut socket, _)) = accepted else {
            return;
        };

        let request = read_request(&mut socket).await;
        *captured_for_task.lock().await = Some(request);
        request_received_for_task.notify_one();

        if hold_body_open {
            let chunk = "data: {}\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n{:X}\r\n",
                chunk.len()
            );
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            if socket.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
            if socket.write_all(b"\r\n").await.is_err() {
                return;
            }
            response_started_for_task.notify_one();
            tokio::select! {
                _ = &mut shutdown_rx => {},
                _ = &mut release_rx => {},
            };
        } else {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            response_started_for_task.notify_one();
        }
    });

    LoopbackServer {
        base_url: format!("http://{address}"),
        captured,
        request_received,
        response_started,
        shutdown: Some(shutdown_tx),
        release_body: hold_body_open.then_some(release_tx),
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let count = socket
            .read(&mut buffer)
            .await
            .expect("read request headers");
        assert!(count > 0, "client closed before request headers");
        raw.extend_from_slice(&buffer[..count]);
        if let Some(position) = find_subsequence(&raw, b"\r\n\r\n") {
            break position + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    while raw.len() < header_end + content_length {
        let count = socket.read(&mut buffer).await.expect("read request body");
        assert!(count > 0, "client closed before request body");
        raw.extend_from_slice(&buffer[..count]);
    }
    let body = String::from_utf8_lossy(&raw[header_end..header_end + content_length]).to_string();
    CapturedRequest { method, path, body }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl LoopbackServer {
    async fn wait_for_request(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.request_received.notified())
            .await
            .expect("loopback request was received");
    }

    async fn wait_for_response(&self) {
        // Workspace-wide test execution can cold-start reqwest's transport
        // while other integration binaries are active. Keep a bounded
        // startup guard, but do not make it a false failure under load.
        tokio::time::timeout(Duration::from_secs(10), self.response_started.notified())
            .await
            .expect("loopback response was started");
    }

    async fn request(&self) -> CapturedRequest {
        self.wait_for_request().await;
        self.captured
            .lock()
            .await
            .clone()
            .expect("loopback request capture")
    }

    fn stop(mut self) {
        if let Some(release) = self.release_body.take() {
            let _ = release.send(());
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn model(api: &str, provider: &str, base_url: &str) -> Model {
    let mut model = Model::new("loopback-model", "loopback-model", api, provider);
    model.base_url = base_url.to_string();
    model
}

fn context() -> Context {
    Context {
        messages: vec![Message::User(UserContent::string("hello", 1))],
        ..Default::default()
    }
}

fn stream_options(
    hook: Option<OnPayloadFn>,
    abort_signal: Option<Arc<AtomicBool>>,
) -> StreamOptions {
    StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some("secret".to_string()),
            ..Default::default()
        },
        on_payload: hook,
        abort_signal,
        ..Default::default()
    }
}

fn timeout_stream_options(timeout_ms: u64) -> StreamOptions {
    let mut options = stream_options(None, None);
    options.base.timeout_ms = Some(timeout_ms);
    options
}

fn payload_hook() -> (OnPayloadFn, Arc<AtomicBool>) {
    let called = Arc::new(AtomicBool::new(false));
    let called_for_hook = Arc::clone(&called);
    let hook: OnPayloadFn = Arc::new(move |mut payload, _model| -> OnPayloadFuture {
        called_for_hook.store(true, Ordering::SeqCst);
        Box::pin(async move {
            payload["transport_hook"] = json!("payload-hook");
            Some(payload)
        })
    });
    (hook, called)
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content()
        .iter()
        .filter_map(|block| match block {
            pi_ai::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn assert_success(
    server: LoopbackServer,
    stream: AssistantMessageEventStream,
    hook_called: Arc<AtomicBool>,
    expected_path: &str,
) {
    let (events, message) = stream.collect().await;
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(text_of(&message), "ok");
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Done {
            reason: pi_ai::types::DoneReason::Stop,
            ..
        }
    )));
    assert!(
        hook_called.load(Ordering::SeqCst),
        "on_payload was not called"
    );

    let request = server.request().await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, expected_path);
    let body: Value = serde_json::from_str(&request.body).expect("JSON request body");
    assert_eq!(body["transport_hook"], "payload-hook");
    server.stop();
}

async fn assert_aborted(
    server: LoopbackServer,
    stream: AssistantMessageEventStream,
    signal: Arc<AtomicBool>,
) {
    server.wait_for_response().await;
    signal.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let (events, message) = tokio::time::timeout(Duration::from_secs(2), stream.collect())
        .await
        .expect("aborted stream did not terminate");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
    assert!(message
        .error_message()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("aborted"));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            ..
        }
    )));
    server.stop();
}

async fn assert_pre_aborted(stream: AssistantMessageEventStream) {
    let (events, message) = tokio::time::timeout(Duration::from_secs(2), stream.collect())
        .await
        .expect("pre-aborted stream did not terminate");
    assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
    assert!(message
        .error_message()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("aborted"));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            ..
        }
    )));
}

async fn assert_timed_out(server: LoopbackServer, stream: AssistantMessageEventStream) {
    server.wait_for_response().await;
    let (events, message) = tokio::time::timeout(Duration::from_secs(2), stream.collect())
        .await
        .expect("request timeout must settle the stream");
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    let error = message.error_message().unwrap_or_default();
    assert!(
        error.to_ascii_lowercase().contains("timed out"),
        "unexpected timeout error: {error}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::Error { .. }))
            .count(),
        1
    );
    server.stop();
}

fn openai_completions_sse() -> &'static str {
    "data: {\"id\":\"loopback\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
}

fn responses_sse() -> &'static str {
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
}

fn anthropic_sse() -> &'static str {
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"loopback\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
}

fn google_sse() -> &'static str {
    "data: {\"responseId\":\"loopback\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1,\"totalTokenCount\":2}}\n\n"
}

#[tokio::test(flavor = "current_thread")]
async fn payload_hook_reaches_wire_for_every_target_adaptor() {
    let context = context();

    let server = spawn_loopback(openai_completions_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = openai_completions::stream(
        &model("openai-completions", "openai", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &OpenAIChatOptions {
            base: stream_options(Some(hook), None),
            ..Default::default()
        },
    );
    assert_success(server, stream, called, "/chat/completions").await;

    let server = spawn_loopback(responses_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = openai_responses::stream(
        &model("openai-responses", "openai", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &OpenAIResponsesOptions {
            base: stream_options(Some(hook), None),
            ..Default::default()
        },
    );
    assert_success(server, stream, called, "/responses").await;

    let server = spawn_loopback(responses_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = azure_openai_responses::stream(
        &model(
            "azure-openai-responses",
            "azure-openai-responses",
            &base_url,
        ),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &AzureOpenAIResponsesOptions {
            base: stream_options(Some(hook), None),
            azure_base_url: Some(base_url),
            azure_deployment_name: Some("loopback".to_string()),
            ..Default::default()
        },
    );
    assert_success(
        server,
        stream,
        called,
        "/deployments/loopback/responses?api-version=v1",
    )
    .await;

    let server = spawn_loopback(anthropic_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = anthropic_messages::stream(
        &model("anthropic-messages", "anthropic", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &AnthropicOptions {
            base: stream_options(Some(hook), None),
            ..Default::default()
        },
    );
    assert_success(server, stream, called, "/v1/messages").await;

    let server = spawn_loopback(google_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = google_generative_ai::stream(
        &model("google-generative-ai", "google", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &GoogleOptions {
            base: stream_options(Some(hook), None),
            tool_choice: None,
            thinking: None,
        },
    );
    assert_success(
        server,
        stream,
        called,
        "/models/loopback-model:streamGenerateContent?alt=sse",
    )
    .await;

    let server = spawn_loopback(google_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = google_vertex::stream(
        &model("google-vertex", "google-vertex", &base_url),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &GoogleVertexOptions {
            base: stream_options(Some(hook), None),
            ..Default::default()
        },
    );
    assert_success(
        server,
        stream,
        called,
        "/v1/publishers/google/models/loopback-model:streamGenerateContent?alt=sse",
    )
    .await;

    let server = spawn_loopback(openai_completions_sse(), false);
    let base_url = server.base_url.clone();
    let (hook, called) = payload_hook();
    let stream = mistral_conversations::stream(
        &model("mistral-conversations", "mistral", &base_url),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &MistralOptions {
            base: stream_options(Some(hook), None),
            ..Default::default()
        },
    );
    assert_success(server, stream, called, "/v1/chat/completions").await;
}

#[tokio::test(flavor = "current_thread")]
async fn abort_signal_cancels_body_read_for_every_target_adaptor() {
    let context = context();

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = openai_completions::stream(
        &model("openai-completions", "openai", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &OpenAIChatOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            ..Default::default()
        },
    );
    assert_aborted(server, stream, signal).await;

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = openai_responses::stream(
        &model("openai-responses", "openai", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &OpenAIResponsesOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            ..Default::default()
        },
    );
    assert_aborted(server, stream, signal).await;

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = azure_openai_responses::stream(
        &model(
            "azure-openai-responses",
            "azure-openai-responses",
            &base_url,
        ),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &AzureOpenAIResponsesOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            azure_base_url: Some(base_url),
            azure_deployment_name: Some("loopback".to_string()),
            ..Default::default()
        },
    );
    assert_aborted(server, stream, signal).await;

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = anthropic_messages::stream(
        &model("anthropic-messages", "anthropic", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &AnthropicOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            ..Default::default()
        },
    );
    assert_aborted(server, stream, signal).await;

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = google_generative_ai::stream(
        &model("google-generative-ai", "google", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &GoogleOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            tool_choice: None,
            thinking: None,
        },
    );
    assert_aborted(server, stream, signal).await;

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = google_vertex::stream(
        &model("google-vertex", "google-vertex", &base_url),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &GoogleVertexOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            ..Default::default()
        },
    );
    assert_aborted(server, stream, signal).await;

    let signal = Arc::new(AtomicBool::new(false));
    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = mistral_conversations::stream(
        &model("mistral-conversations", "mistral", &base_url),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &MistralOptions {
            base: stream_options(None, Some(Arc::clone(&signal))),
            ..Default::default()
        },
    );
    assert_aborted(server, stream, signal).await;
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_terminates_an_inflight_body_read_exactly_once() {
    let context = context();

    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = openai_completions::stream(
        &model("openai-completions", "openai", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &OpenAIChatOptions {
            base: timeout_stream_options(25),
            ..Default::default()
        },
    );
    assert_timed_out(server, stream).await;

    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = openai_responses::stream(
        &model("openai-responses", "openai", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &OpenAIResponsesOptions {
            base: timeout_stream_options(25),
            ..Default::default()
        },
    );
    assert_timed_out(server, stream).await;

    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = azure_openai_responses::stream(
        &model(
            "azure-openai-responses",
            "azure-openai-responses",
            &base_url,
        ),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &AzureOpenAIResponsesOptions {
            base: timeout_stream_options(25),
            azure_base_url: Some(base_url),
            azure_deployment_name: Some("loopback".to_string()),
            ..Default::default()
        },
    );
    assert_timed_out(server, stream).await;

    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = anthropic_messages::stream(
        &model("anthropic-messages", "anthropic", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &AnthropicOptions {
            base: timeout_stream_options(25),
            ..Default::default()
        },
    );
    assert_timed_out(server, stream).await;

    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = google_generative_ai::stream(
        &model("google-generative-ai", "google", &base_url),
        &context,
        reqwest::Client::new(),
        &base_url,
        Some("secret"),
        &GoogleOptions {
            base: timeout_stream_options(25),
            tool_choice: None,
            thinking: None,
        },
    );
    assert_timed_out(server, stream).await;

    let server = spawn_loopback("", true);
    let base_url = server.base_url.clone();
    let stream = google_vertex::stream(
        &model("google-vertex", "google-vertex", &base_url),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &GoogleVertexOptions {
            base: timeout_stream_options(25),
            ..Default::default()
        },
    );
    assert_timed_out(server, stream).await;
}

#[tokio::test(flavor = "current_thread")]
async fn pre_aborted_signal_is_terminal_for_every_target_adaptor() {
    let context = context();
    let aborted_options = || stream_options(None, Some(Arc::new(AtomicBool::new(true))));

    assert_pre_aborted(openai_completions::stream(
        &model("openai-completions", "openai", "http://127.0.0.1:1"),
        &context,
        reqwest::Client::new(),
        "http://127.0.0.1:1",
        Some("secret"),
        &OpenAIChatOptions {
            base: aborted_options(),
            ..Default::default()
        },
    ))
    .await;

    assert_pre_aborted(openai_responses::stream(
        &model("openai-responses", "openai", "http://127.0.0.1:1"),
        &context,
        reqwest::Client::new(),
        "http://127.0.0.1:1",
        Some("secret"),
        &OpenAIResponsesOptions {
            base: aborted_options(),
            ..Default::default()
        },
    ))
    .await;

    assert_pre_aborted(azure_openai_responses::stream(
        &model(
            "azure-openai-responses",
            "azure-openai-responses",
            "http://127.0.0.1:1",
        ),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &AzureOpenAIResponsesOptions {
            base: aborted_options(),
            azure_base_url: Some("http://127.0.0.1:1".to_string()),
            azure_deployment_name: Some("loopback".to_string()),
            ..Default::default()
        },
    ))
    .await;

    assert_pre_aborted(anthropic_messages::stream(
        &model("anthropic-messages", "anthropic", "http://127.0.0.1:1"),
        &context,
        reqwest::Client::new(),
        "http://127.0.0.1:1",
        Some("secret"),
        &AnthropicOptions {
            base: aborted_options(),
            ..Default::default()
        },
    ))
    .await;

    assert_pre_aborted(google_generative_ai::stream(
        &model("google-generative-ai", "google", "http://127.0.0.1:1"),
        &context,
        reqwest::Client::new(),
        "http://127.0.0.1:1",
        Some("secret"),
        &GoogleOptions {
            base: aborted_options(),
            tool_choice: None,
            thinking: None,
        },
    ))
    .await;

    assert_pre_aborted(google_vertex::stream(
        &model("google-vertex", "google-vertex", "http://127.0.0.1:1"),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &GoogleVertexOptions {
            base: aborted_options(),
            ..Default::default()
        },
    ))
    .await;

    assert_pre_aborted(mistral_conversations::stream(
        &model("mistral-conversations", "mistral", "http://127.0.0.1:1"),
        &context,
        reqwest::Client::new(),
        Some("secret"),
        &MistralOptions {
            base: aborted_options(),
            ..Default::default()
        },
    ))
    .await;
}

#[test]
fn openai_completions_replays_orphans_and_chat_template_reasoning_fields() {
    let mut model = model("loopback-model", "openai", "http://127.0.0.1:1");
    model.reasoning = true;
    model.thinking_level_map = Some(BTreeMap::from([(
        pi_ai::types::ModelThinkingLevel::High,
        Some("mapped-high".to_string()),
    )]));

    let mut assistant = AssistantMessage::new();
    assistant.set_content(vec![pi_ai::types::ContentBlock::tool_call(
        "call-1",
        "lookup",
        json!({"query":"rust"}),
    )]);
    let context = Context {
        messages: vec![
            Message::Assistant(assistant),
            Message::User(UserContent::string("continue", 2)),
        ],
        ..Default::default()
    };

    let transformed = openai_completions::transform_messages(&model, &context.messages);
    assert_eq!(transformed.len(), 3);
    let Message::ToolResult(synthetic) = &transformed[1] else {
        panic!("orphaned tool call was not followed by a synthetic result");
    };
    assert_eq!(synthetic.tool_call_id(), "call-1");
    assert_eq!(synthetic.tool_name(), "lookup");
    assert!(synthetic.is_error());
    assert!(matches!(
        synthetic.content().first(),
        Some(pi_ai::types::ContentBlock::Text { text, .. }) if text == "No result provided"
    ));

    let compat = openai_completions::OpenAiCompletionsCompat::get(&model);
    let params = openai_completions::build_params(&model, &context, None, &compat, "none")
        .expect("completions params with synthetic result");
    assert_eq!(params["messages"][1]["role"], "tool");
    assert_eq!(params["messages"][1]["tool_call_id"], "call-1");
    assert_eq!(params["messages"][1]["content"], "No result provided");

    let mut aborted_assistant = AssistantMessage::new();
    aborted_assistant.set_stop_reason(StopReason::Aborted);
    aborted_assistant.set_content(vec![pi_ai::types::ContentBlock::text("partial")]);
    let transformed = openai_completions::transform_messages(
        &model,
        &[
            Message::User(UserContent::string("start", 3)),
            Message::Assistant(aborted_assistant),
        ],
    );
    assert_eq!(transformed.len(), 1);
    assert!(matches!(transformed[0], Message::User(_)));

    let mut options = StreamOptions {
        sampling_params: Some(json!({
            "reasoningEffort": "high",
            "thinkingBudget": 123,
        })),
        ..Default::default()
    };

    model.compat = Some(json!({"thinkingFormat":"qwen-chat-template"}));
    let compat = openai_completions::OpenAiCompletionsCompat::get(&model);
    let params = openai_completions::build_params(
        &model,
        &Context::default(),
        Some(&options),
        &compat,
        "none",
    )
    .expect("qwen chat-template params");
    assert_eq!(
        params["chat_template_kwargs"],
        json!({"enable_thinking":true,"preserve_thinking":true})
    );

    model.compat = Some(json!({
        "thinkingFormat":"chat-template",
        "chatTemplateKwargs":{
            "enabled":{"$var":"thinking.enabled"},
            "effort":{"$var":"thinking.effort"},
            "budget":{"$var":"thinking.budget"},
            "constant":true
        }
    }));
    let compat = openai_completions::OpenAiCompletionsCompat::get(&model);
    let params = openai_completions::build_params(
        &model,
        &Context::default(),
        Some(&options),
        &compat,
        "none",
    )
    .expect("configurable chat-template params");
    assert_eq!(
        params["chat_template_kwargs"],
        json!({
            "enabled":true,
            "effort":"mapped-high",
            "budget":123,
            "constant":true
        })
    );

    model.compat = Some(json!({
        "thinkingFormat":"baseten",
        "supportsReasoningEffort":true,
        "chatTemplateArgs":{
            "enabled":{"$var":"thinking.enabled"},
            "effort":{"$var":"thinking.effort"}
        }
    }));
    let compat = openai_completions::OpenAiCompletionsCompat::get(&model);
    let params = openai_completions::build_params(
        &model,
        &Context::default(),
        Some(&options),
        &compat,
        "none",
    )
    .expect("Baseten chat-template params");
    assert_eq!(
        params["chat_template_args"],
        json!({"enabled":true,"effort":"mapped-high"})
    );
    assert_eq!(params["reasoning_effort"], "mapped-high");

    options.sampling_params = Some(json!({"thinkingBudget":123}));
    model.compat = Some(json!({
        "thinkingFormat":"chat-template",
        "chatTemplateKwargs":{
            "optional":{"$var":"thinking.effort","omitWhenOff":true}
        }
    }));
    let compat = openai_completions::OpenAiCompletionsCompat::get(&model);
    let params = openai_completions::build_params(
        &model,
        &Context::default(),
        Some(&options),
        &compat,
        "none",
    )
    .expect("chat-template off params");
    assert!(!params["chat_template_kwargs"].is_object());
}
