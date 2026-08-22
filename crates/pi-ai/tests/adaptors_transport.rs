//! Transport-level tests for the mistral-conversations and
//! openai-codex-responses adaptors, driven by a local TCP HTTP server.
//!
//! These are the Rust analogs of the upstream HTTP-transport tests
//! (`packages/ai/test/mistral-http-transport.test.ts` and the SSE surface of
//! `packages/ai/test/openai-codex-stream.test.ts`): the wire request
//! (URL, headers, JSON body), the streaming result, and the error surfaces
//! are all observable through a real reqwest request.

use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pi_ai::api::mistral_conversations::MistralOptions;
use pi_ai::api::openai_codex_responses::OpenAICodexResponsesOptions;
use pi_ai::types::{
    Context, ContentBlock, Message, ProviderRequestOptions, StopReason, StreamOptions, UserContent,
};

/// A captured HTTP request: method, path, headers, body.
#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

/// A tiny local HTTP/1.1 server that captures each request and answers with a
/// canned response. Returns the base URL and a shutdown sender.
fn spawn_local_server(
    response: String,
    captured: std::sync::Arc<tokio::sync::Mutex<Option<CapturedRequest>>>,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local server");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { continue };
                    let req = read_request(&mut socket).await;
                    *captured.lock().await = Some(req);
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            }
        }
    });
    (format!("http://{address}"), shutdown_tx)
}

/// Read one HTTP request (headers + content-length body).
async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut buf).await.expect("read request");
        if n == 0 {
            break None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_subsequence(&raw, b"\r\n\r\n") {
            break Some(pos + 4);
        }
    };
    let header_end = header_end.expect("request headers");
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = BTreeMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    let mut body = String::from_utf8_lossy(&raw[header_end..]).to_string();
    while body.len() < content_length {
        let n = socket.read(&mut buf).await.expect("read body");
        if n == 0 {
            break;
        }
        body.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    CapturedRequest { method, path, headers, body }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Build a plain HTTP response from a status line + headers + body.
fn simple_response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    format!("{head}{body}")
}

fn text_of(message: &pi_ai::types::AssistantMessage) -> String {
    message
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Mistral transport
// ---------------------------------------------------------------------------

fn mistral_model() -> pi_ai::Model {
    let models = pi_ai::providers::catalog_models("mistral");
    models
        .into_iter()
        .find(|m| m.id == "mistral-large-latest")
        .expect("mistral-large-latest in catalog")
}

fn mistral_terminal_sse() -> String {
    "data: {\"id\":\"mistral-response-id\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{\"content\":\"Hello\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\r\n\r\ndata: [DONE]\r\n\r\n"
        .to_string()
}

#[test]
fn mistral_full_transport_roundtrip() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(None::<CapturedRequest>));
        let response = simple_response(
            "200 OK",
            &[("content-type", "text/event-stream"), ("x-request-id", "request-1")],
            &mistral_terminal_sse(),
        );
        let (server_base, shutdown_tx) = spawn_local_server(response, captured.clone());

        let mut model = mistral_model();
        model.base_url = server_base;
        let context = Context {
            system_prompt: Some("Be precise".to_string()),
            messages: vec![Message::User(UserContent::string("describe", 1))],
            ..Default::default()
        };
        let mut headers = pi_ai::types::ProviderHeaders::new();
        headers.insert("x-custom".to_string(), Some("value".to_string()));
        let request_options = ProviderRequestOptions {
            api_key: Some("secret".to_string()),
            headers: Some(headers),
            ..Default::default()
        };
        let options = MistralOptions {
            base: StreamOptions {
                base: request_options,
                temperature: Some(0.9),
                max_tokens: Some(123),
                session_id: Some("session-1".to_string()),
                ..Default::default()
            },
            prompt_mode: Some("reasoning".to_string()),
            reasoning_effort: Some("high".to_string()),
            tool_choice: Some(serde_json::json!({ "type": "function", "function": { "name": "lookup" } })),
        };
        let stream = pi_ai::api::mistral_conversations::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some("secret"),
            &options,
        );
        let message = stream.collect().await.1;
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        assert_eq!(text_of(&message), "Hello");
        assert_eq!(message.usage().unwrap().input, 2);
        assert_eq!(message.usage().unwrap().output, 1);
        assert_eq!(message.response_id().unwrap(), "mistral-response-id");

        let req = captured.lock().await.take().expect("server received a request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/chat/completions");
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer secret");
        assert_eq!(req.headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(req.headers.get("x-affinity").unwrap(), "session-1");
        assert_eq!(req.headers.get("x-custom").unwrap(), "value");
        assert!(req.headers.get("user-agent").unwrap().starts_with("pi ("));

        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "mistral-large-latest");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 123);
        assert_eq!(body["prompt_mode"], "reasoning");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["prompt_cache_key"], "session-1");
        assert_eq!(body["temperature"], 0.9);
        assert_eq!(body["tool_choice"], serde_json::json!({ "type": "function", "function": { "name": "lookup" } }));
        assert!(!body.as_object().unwrap().contains_key("maxTokens"));
        assert!(!body.as_object().unwrap().contains_key("promptMode"));
        assert!(!body.as_object().unwrap().contains_key("promptCacheKey"));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "Be precise");
        assert_eq!(messages[1]["role"], "user");
        let _ = shutdown_tx.send(());
    });
}

#[test]
fn mistral_http_error_surface() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(None::<CapturedRequest>));
        let response = simple_response("403 Forbidden", &[], r#"{"message":"blocked by gateway"}"#);
        let (server_base, shutdown_tx) = spawn_local_server(response, captured);

        let mut model = mistral_model();
        model.base_url = server_base;
        let request_options = ProviderRequestOptions {
            api_key: Some("secret".to_string()),
            ..Default::default()
        };
        let stream = pi_ai::api::mistral_conversations::stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            Some("secret"),
            &MistralOptions { base: StreamOptions { base: request_options, ..Default::default() }, ..Default::default() },
        );
        let message = stream.collect().await.1;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert_eq!(
            message.error_message().unwrap_or(""),
            "Mistral API error (403): {\"message\":\"blocked by gateway\"}"
        );
        let _ = shutdown_tx.send(());
    });
}

#[test]
fn mistral_timeout_while_waiting_for_stream() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        // A server that accepts the connection and never responds: the reqwest
        // request-level timeout must surface as an error stream.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((mut socket, _)) = accepted else { continue };
                        // Read the request, then never respond.
                        let _ = read_request(&mut socket).await;
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                }
            }
        });

        let mut model = mistral_model();
        model.base_url = format!("http://{address}");
        let request_options = ProviderRequestOptions {
            api_key: Some("secret".to_string()),
            timeout_ms: Some(200),
            ..Default::default()
        };
        let stream = pi_ai::api::mistral_conversations::stream(
            &model,
            &Context::default(),
            reqwest::Client::new(),
            Some("secret"),
            &MistralOptions { base: StreamOptions { base: request_options, ..Default::default() }, ..Default::default() },
        );
        let message = stream.collect().await.1;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        let err = message.error_message().unwrap_or("").to_string();
        assert!(err.to_lowercase().contains("timed out"), "{err}");
        let _ = shutdown_tx.send(());
    });
}

// ---------------------------------------------------------------------------
// Codex transport (SSE)
// ---------------------------------------------------------------------------

fn codex_model() -> pi_ai::Model {
    let models = pi_ai::providers::catalog_models("openai-codex");
    models.into_iter().find(|m| m.id == "gpt-5.5").expect("gpt-5.5 in catalog")
}

fn codex_token(account_id: &str) -> String {
    use base64::Engine;
    let payload = base64::engine::general_purpose::STANDARD.encode(format!(
        "{{\"https://api.openai.com/auth\": {{\"chatgpt_account_id\": \"{account_id}\"}}}}"
    ));
    format!("aaa.{payload}.bbb")
}

fn codex_terminal_sse() -> String {
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}

data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}

data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}

data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"end_turn\":false,\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8,\"input_tokens_details\":{\"cached_tokens\":0}}}}

"
    .to_string()
}

#[test]
fn codex_sse_transport_roundtrip() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(None::<CapturedRequest>));
        let response = simple_response("200 OK", &[("content-type", "text/event-stream")], &codex_terminal_sse());
        let (server_base, shutdown_tx) = spawn_local_server(response, captured.clone());

        let mut model = codex_model();
        model.base_url = server_base;
        let token = codex_token("acc_test");
        let context = Context {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            messages: vec![Message::User(UserContent::string("Say hello", 1))],
            ..Default::default()
        };
        let request_options = ProviderRequestOptions {
            api_key: Some(token.clone()),
            ..Default::default()
        };
        let options = OpenAICodexResponsesOptions {
            base: StreamOptions {
                base: request_options,
                session_id: Some("test-session-123".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let stream = pi_ai::api::openai_codex_responses::stream(
            &model,
            &context,
            reqwest::Client::new(),
            Some(&token),
            &options,
        );
        let message = stream.collect().await.1;
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        assert_eq!(text_of(&message), "Hello");
        assert_eq!(message.usage().unwrap().input, 5);
        let pi_ai::types::AssistantMessage::Assistant { end_turn, .. } = &message;
        let end_turn = *end_turn;
        assert_eq!(end_turn, Some(false));

        let req = captured.lock().await.take().expect("server received a request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/codex/responses");
        assert_eq!(req.headers.get("authorization").unwrap(), &format!("Bearer {token}"));
        assert_eq!(req.headers.get("chatgpt-account-id").unwrap(), "acc_test");
        assert_eq!(req.headers.get("originator").unwrap(), "pi");
        assert_eq!(req.headers.get("openai-beta").unwrap(), "responses=experimental");
        assert_eq!(req.headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(req.headers.get("session-id").unwrap(), "test-session-123");
        assert_eq!(req.headers.get("x-client-request-id").unwrap(), "test-session-123");
        assert!(!req.headers.contains_key("content-encoding"));

        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "You are a helpful assistant.");
        assert_eq!(body["prompt_cache_key"], "test-session-123");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
        let _ = shutdown_tx.send(());
    });
}
