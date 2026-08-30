#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic wire and stream fixtures for the Google Generative AI adapter.
//!
//! The fixture peer is a real loopback TCP server. It observes the generated
//! URL, headers, and JSON request, then returns provider-shaped SSE or error
//! data so the public adapter entry point is exercised end to end.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use pi_ai::api::google_generative_ai::{self, GoogleOptions, GoogleThinking};
use pi_ai::model::{Model, ModelInput};
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorReason, Message,
    ProviderHeaders, ProviderRequestOptions, StopReason, StreamOptions, Tool, UserContent,
};

#[derive(Debug)]
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
        let read = socket.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "fixture peer closed before request headers");
        raw.extend_from_slice(&chunk[..read]);
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();
    let mut headers = BTreeMap::new();
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        headers.insert(name, value);
    }

    while raw.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "fixture peer closed before request body");
        raw.extend_from_slice(&chunk[..read]);
    }

    CapturedRequest {
        method,
        path,
        headers,
        body: raw[header_end..header_end + content_length].to_vec(),
    }
}

async fn fixture_server(
    status: u16,
    body: Vec<u8>,
) -> (String, oneshot::Receiver<CapturedRequest>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let (request_tx, request_rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept fixture request");
        let request = read_request(&mut socket).await;
        let reason = if status == 200 { "OK" } else { "Bad Request" };
        let response_head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(response_head.as_bytes())
            .await
            .expect("write fixture response headers");
        socket
            .write_all(&body)
            .await
            .expect("write fixture response body");
        let _ = request_tx.send(request);
    });

    (format!("http://{address}"), request_rx)
}

fn sse(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.into_bytes()
}

fn success_fixture() -> Vec<u8> {
    sse([
        json!({
            "responseId": "google-response-1",
            "candidates": [{
                "content": {"parts": [{
                    "text": "consider",
                    "thought": true,
                    "thoughtSignature": "c2ln"
                }]}
            }]
        }),
        json!({
            "candidates": [{
                "content": {"parts": [{"text": "answer"}]}
            }]
        }),
        json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {
                        "name": "lookup",
                        "args": {"key": "value"},
                        "id": "call-google-1"
                    },
                    "thoughtSignature": "dG9vbA=="
                }]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "cachedContentTokenCount": 2,
                "candidatesTokenCount": 3,
                "thoughtsTokenCount": 2,
                "totalTokenCount": 15
            }
        }),
    ])
}

fn google_model() -> Model {
    let mut model = Model::new(
        "gemini-2.5-pro",
        "Gemini 2.5 Pro",
        "google-generative-ai",
        "google",
    );
    model.reasoning = true;
    model.input = vec![ModelInput::Text, ModelInput::Image];
    model
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

fn options() -> GoogleOptions {
    let mut headers = ProviderHeaders::new();
    headers.insert("x-fixture-header".to_string(), Some("google".to_string()));
    GoogleOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                api_key: Some("fixture-key".to_string()),
                headers: Some(headers),
                max_retries: Some(0),
                ..Default::default()
            },
            temperature: Some(0.25),
            max_tokens: Some(64),
            ..Default::default()
        },
        tool_choice: Some("auto".to_string()),
        thinking: Some(GoogleThinking {
            enabled: true,
            budget_tokens: Some(8192),
            level: None,
        }),
    }
}

async fn run_fixture(
    status: u16,
    body: Vec<u8>,
) -> (
    Vec<AssistantMessageEvent>,
    AssistantMessage,
    CapturedRequest,
) {
    let (base_url, request_rx) = fixture_server(status, body).await;
    let stream = google_generative_ai::stream(
        &google_model(),
        &context_with_tool(),
        reqwest::Client::new(),
        &base_url,
        Some("fixture-key"),
        &options(),
    );
    let (events, message) = stream.collect().await;
    let request = request_rx.await.expect("fixture request capture");
    (events, message, request)
}

#[tokio::test]
async fn google_success_fixture_preserves_wire_headers_reasoning_tools_usage_and_partials() {
    let (events, message, request) = run_fixture(200, success_fixture()).await;

    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        "/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        request.headers.get("x-goog-api-key"),
        Some(&"fixture-key".to_string())
    );
    assert_eq!(
        request.headers.get("x-fixture-header"),
        Some(&"google".to_string())
    );
    assert!(request
        .headers
        .get("user-agent")
        .is_some_and(|value| !value.is_empty()));

    let body: Value = serde_json::from_slice(&request.body).expect("Google request JSON");
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "inspect this");
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be exact");
    assert_eq!(body["generationConfig"]["temperature"], 0.25);
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
    assert_eq!(body["thinkingConfig"]["includeThoughts"], true);
    assert_eq!(body["thinkingConfig"]["thinkingBudget"], 8192);
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );

    assert_eq!(message.response_id(), Some("google-response-1"));
    assert_eq!(message.stop_reason(), Some(StopReason::ToolUse));
    assert_eq!(message.usage().map(|usage| usage.input), Some(8));
    assert_eq!(message.usage().map(|usage| usage.output), Some(5));
    assert_eq!(message.usage().map(|usage| usage.reasoning), Some(Some(2)));
    assert_eq!(message.usage().map(|usage| usage.cache_read), Some(2));
    assert!(message.content().iter().any(
        |block| matches!(block, ContentBlock::Thinking { thinking, thinking_signature, .. }
            if thinking == "consider" && thinking_signature.as_deref() == Some("c2ln"))
    ));
    assert!(message
        .content()
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "answer")));
    assert!(message.content().iter().any(
        |block| matches!(block, ContentBlock::ToolCall { id, name, arguments, thought_signature, .. }
            if id == "call-google-1"
                && name == "lookup"
                && arguments["key"] == "value"
                && thought_signature.as_deref() == Some("dG9vbA=="))
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Done {
            reason: pi_ai::types::DoneReason::ToolUse,
            ..
        }
    )));

    let text_partial_has_content = events.iter().any(|event| {
        matches!(
            event,
            AssistantMessageEvent::TextDelta { partial, .. }
                if partial.content().iter().any(
                    |block| matches!(block, ContentBlock::Text { text, .. } if text == "answer")
                )
        )
    });
    let tool_partial_has_content = events.iter().any(|event| {
        matches!(
            event,
            AssistantMessageEvent::ToolCallStart { partial, .. }
                if partial.content().iter().any(
                    |block| matches!(block, ContentBlock::ToolCall { id, .. } if id == "call-google-1")
                )
        )
    });
    assert!(text_partial_has_content, "text partial lost live content");
    assert!(tool_partial_has_content, "tool partial lost live content");
}

#[tokio::test]
async fn google_malformed_sse_fixture_is_a_redacted_terminal_error() {
    let (events, message, request) =
        run_fixture(200, b"data: {malformed-google}\n\n".to_vec()).await;

    assert_eq!(
        request.headers.get("x-goog-api-key"),
        Some(&"fixture-key".to_string())
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            ..
        })
    ));
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert!(message
        .error_message()
        .is_some_and(|error| error.starts_with("Malformed Google stream chunk:")));
    assert!(!message
        .error_message()
        .unwrap_or_default()
        .contains("fixture-key"));
}

#[tokio::test]
async fn google_http_error_fixture_preserves_provider_detail_without_key_leakage() {
    let body = br#"{"error":{"message":"invalid request"}}"#.to_vec();
    let (events, message, request) = run_fixture(400, body).await;

    assert_eq!(
        request.headers.get("x-goog-api-key"),
        Some(&"fixture-key".to_string())
    );
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            ..
        })
    ));
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert_eq!(
        message.error_message(),
        Some("Google API error (400): invalid request")
    );
    assert!(!message
        .error_message()
        .unwrap_or_default()
        .contains("fixture-key"));
}
