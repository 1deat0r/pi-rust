#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic local-loopback evidence for the Groq and Hugging Face rows.
//!
//! Both providers intentionally use the shared OpenAI Chat Completions
//! adapter. These tests keep the provider-specific registration, auth, model
//! routing, and wire/runtime evidence explicit without contacting either
//! vendor.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::api::openai_completions::{self, OpenAIChatOptions};
use pi_ai::auth::{ApiKeyCredential, AuthContext};
use pi_ai::event_stream::AssistantMessageEventStream;
use pi_ai::model::Model;
use pi_ai::providers::all::{builtin_providers, catalog_models};
use pi_ai::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message,
    ProviderRequestOptions, StopReason, StreamOptions, Tool, UserContent,
};

#[derive(Debug)]
struct CapturedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
}

struct FixtureHandle {
    base_url: String,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    request_seen: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

struct StreamResult {
    events: Vec<AssistantMessageEvent>,
    message: AssistantMessage,
    requests: Vec<CapturedRequest>,
}

struct PlannedResponse {
    status: u16,
    body: String,
    content_type: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    delay: Duration,
}

impl PlannedResponse {
    fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "text/event-stream",
            headers: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "application/json",
            headers: Vec::new(),
            delay: Duration::ZERO,
        }
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    use tokio::io::AsyncReadExt;

    let mut raw = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "fixture client closed before request headers");
        raw.extend_from_slice(&buffer[..count]);
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let path = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let mut headers = BTreeMap::new();
    let mut content_length = 0;
    for (name, value) in header_text
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
    {
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = value.parse::<usize>().unwrap();
        }
        headers.insert(name, value);
    }
    while raw.len() < header_end + content_length {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "fixture client closed before request body");
        raw.extend_from_slice(&buffer[..count]);
    }
    let body = serde_json::from_slice(&raw[header_end..header_end + content_length]).unwrap();
    CapturedRequest {
        path,
        headers,
        body,
    }
}

async fn spawn_fixture(responses: Vec<PlannedResponse>) -> FixtureHandle {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_task = Arc::clone(&captured);
    let request_seen = Arc::new(AtomicBool::new(false));
    let request_seen_task = Arc::clone(&request_seen);
    let server = tokio::spawn(async move {
        for planned in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            captured_task.lock().unwrap().push(request);
            request_seen_task.store(true, Ordering::Release);
            if !planned.delay.is_zero() {
                tokio::time::sleep(planned.delay).await;
            }
            let reason = match planned.status {
                200 => "OK",
                401 => "Unauthorized",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Fixture",
            };
            let mut response = format!(
                "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                planned.status,
                reason,
                planned.content_type,
                planned.body.len()
            );
            for (name, value) in planned.headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(planned.body.as_bytes()).await;
        }
    });
    FixtureHandle {
        base_url: format!("http://{address}"),
        captured,
        request_seen,
        server,
    }
}

fn model_for(provider: &str) -> Model {
    let model_id = match provider {
        "groq" => "openai/gpt-oss-20b",
        "huggingface" => "moonshotai/Kimi-K2.5",
        _ => unreachable!("provider is bounded to this fixture"),
    };
    catalog_models(provider)
        .into_iter()
        .find(|model| model.id == model_id)
        .unwrap_or_else(|| panic!("missing pinned {provider}/{model_id} model"))
}

fn fixture_context(with_tool: bool) -> Context {
    let tools = if with_tool {
        vec![Tool {
            name: "lookup".to_string(),
            description: "Look up a value".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
            constrained_sampling: None,
        }]
    } else {
        Vec::new()
    };
    Context {
        system_prompt: Some("Be concise".to_string()),
        messages: vec![Message::User(UserContent::string("hello", 1))],
        tools,
    }
}

fn success_sse(model: &str, with_tool: bool) -> String {
    let mut body = String::new();
    let push_data = |body: &mut String, value: serde_json::Value| {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&value).unwrap());
        body.push_str("\n\n");
    };
    push_data(
        &mut body,
        serde_json::json!({
            "id": "fixture-response",
            "model": model,
            "choices": [{"index": 0, "delta": {"content": "Hello"}}]
        }),
    );
    if with_tool {
        push_data(
            &mut body,
            serde_json::json!({
                "id": "fixture-response",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_fixture",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"query\":\"ru"}
                }]}}]
            }),
        );
        push_data(
            &mut body,
            serde_json::json!({
                "id": "fixture-response",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "st\"}"}
                }]}}]
            }),
        );
        push_data(
            &mut body,
            serde_json::json!({
                "id": "fixture-response",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 5, "total_tokens": 12}
            }),
        );
    } else {
        push_data(
            &mut body,
            serde_json::json!({
                "id": "fixture-response",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
            }),
        );
    }
    body.push_str("data: [DONE]\n\n");
    body
}

async fn run_stream(
    model: &Model,
    options: &OpenAIChatOptions,
    responses: Vec<PlannedResponse>,
) -> StreamResult {
    let fixture = spawn_fixture(responses).await;
    let stream: AssistantMessageEventStream = openai_completions::stream(
        model,
        &fixture_context(!model.id.contains("Kimi")),
        reqwest::Client::new(),
        &fixture.base_url,
        Some("fixture-key"),
        options,
    );
    let (events, message) = stream.collect().await;
    fixture.server.await.unwrap();
    let requests = Arc::try_unwrap(fixture.captured)
        .unwrap()
        .into_inner()
        .unwrap();
    StreamResult {
        events,
        message,
        requests,
    }
}

#[test]
fn groq_and_huggingface_registration_auth_and_catalog_rows_match_upstream() {
    let cases = [
        (
            "groq",
            "Groq",
            "Groq API key",
            "GROQ_API_KEY",
            "openai/gpt-oss-20b",
            "fixture-groq-env",
        ),
        (
            "huggingface",
            "Hugging Face",
            "Hugging Face token",
            "HF_TOKEN",
            "moonshotai/Kimi-K2.5",
            "fixture-hf-env",
        ),
    ];

    for (provider_id, name, auth_name, env_name, model_id, env_key) in cases {
        let provider = builtin_providers()
            .into_iter()
            .find(|provider| provider.id == provider_id)
            .unwrap();
        assert_eq!(provider.name, name);
        assert_eq!(provider.auth.api_key.as_ref().unwrap().name(), auth_name);
        assert!(provider.models.iter().any(|model| model.id == model_id));
        let expected_env_name = env_name.to_string();
        let expected_env_key = env_key.to_string();
        let auth_context = AuthContext {
            env: Arc::new(move |name| {
                (name == expected_env_name).then(|| expected_env_key.clone())
            }),
            file_exists: Arc::new(|_| false),
        };
        let auth = provider.auth.api_key.as_ref().unwrap();
        let from_env = auth.resolve(&auth_context, None).unwrap();
        assert_eq!(from_env.auth.api_key.as_deref(), Some(env_key));
        assert_eq!(from_env.source.as_deref(), Some(env_name));
        let stored = ApiKeyCredential {
            key: Some("fixture-stored".to_string()),
            env: None,
        };
        let from_store = auth.resolve(&auth_context, Some(&stored)).unwrap();
        assert_eq!(from_store.auth.api_key.as_deref(), Some("fixture-stored"));
        assert_eq!(from_store.source.as_deref(), Some("stored credential"));
    }
}

#[tokio::test]
async fn groq_stream_preserves_headers_reasoning_tools_usage_and_event_order() {
    let model = model_for("groq");
    let options = OpenAIChatOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                headers: Some(BTreeMap::from([(
                    "x-fixture-row".to_string(),
                    Some("groq".to_string()),
                )])),
                max_retries: Some(0),
                ..Default::default()
            },
            max_tokens: Some(321),
            temperature: Some(0.2),
            ..Default::default()
        },
        reasoning_effort: Some("high".to_string()),
        ..Default::default()
    };
    let response = PlannedResponse::sse(success_sse(&model.id, true));
    let result = run_stream(&model, &options, vec![response]).await;
    let request = &result.requests[0];
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization"),
        Some(&"Bearer fixture-key".to_string())
    );
    assert_eq!(
        request.headers.get("x-fixture-row"),
        Some(&"groq".to_string())
    );
    assert_eq!(request.body["model"], model.id);
    assert_eq!(request.body["max_completion_tokens"], 321);
    assert_eq!(request.body["temperature"], 0.2);
    assert_eq!(request.body["reasoning_effort"], "high");
    assert_eq!(request.body["stream_options"]["include_usage"], true);
    assert_eq!(request.body["store"], false);
    assert_eq!(request.body["tools"][0]["function"]["name"], "lookup");
    assert_eq!(result.message.stop_reason(), Some(StopReason::ToolUse));
    assert_eq!(result.message.response_id(), Some("fixture-response"));
    assert_eq!(result.message.usage().unwrap().total_tokens, 12);
    assert!(result.message.content().iter().any(|block| matches!(
        block,
        ContentBlock::Text { text, .. } if text == "Hello"
    )));
    assert!(result.message.content().iter().any(|block| matches!(
        block,
        ContentBlock::ToolCall { id, name, .. } if id == "call_fixture" && name == "lookup"
    )));
    let start = result
        .events
        .iter()
        .position(|event| matches!(event, AssistantMessageEvent::Start { .. }))
        .unwrap();
    let text = result
        .events
        .iter()
        .position(|event| matches!(event, AssistantMessageEvent::TextDelta { .. }))
        .unwrap();
    let tool = result
        .events
        .iter()
        .position(|event| matches!(event, AssistantMessageEvent::ToolCallStart { .. }))
        .unwrap();
    let done = result
        .events
        .iter()
        .position(|event| matches!(event, AssistantMessageEvent::Done { .. }))
        .unwrap();
    assert!(start < text && text < tool && tool < done);
}

#[tokio::test]
async fn huggingface_stream_uses_model_route_and_reasoning_options() {
    let model = model_for("huggingface");
    let options = OpenAIChatOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                headers: Some(BTreeMap::from([(
                    "x-fixture-row".to_string(),
                    Some("huggingface".to_string()),
                )])),
                max_retries: Some(0),
                ..Default::default()
            },
            max_tokens: Some(777),
            ..Default::default()
        },
        reasoning_effort: Some("medium".to_string()),
        ..Default::default()
    };
    let response = PlannedResponse::sse(success_sse(&model.id, false));
    let result = run_stream(&model, &options, vec![response]).await;
    let request = &result.requests[0];
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(
        request.headers.get("authorization"),
        Some(&"Bearer fixture-key".to_string())
    );
    assert_eq!(
        request.headers.get("x-fixture-row"),
        Some(&"huggingface".to_string())
    );
    assert_eq!(request.body["model"], "moonshotai/Kimi-K2.5");
    assert_eq!(request.body["max_completion_tokens"], 777);
    assert_eq!(request.body["reasoning_effort"], "medium");
    assert_eq!(request.body["messages"][0]["role"], "system");
    assert_eq!(request.body["store"], false);
    assert_eq!(result.message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(result.message.usage().unwrap().total_tokens, 9);
}

#[tokio::test]
async fn both_rows_encode_truncated_and_non_success_streams_without_key_leaks() {
    for provider in ["groq", "huggingface"] {
        let model = model_for(provider);
        let options = OpenAIChatOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    max_retries: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let truncated = PlannedResponse::sse(
            "data: {\"id\":\"truncated\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        );
        let result = run_stream(&model, &options, vec![truncated]).await;
        assert_eq!(result.message.stop_reason(), Some(StopReason::Error));
        assert!(result
            .message
            .error_message()
            .unwrap_or_default()
            .contains("Stream ended without finish_reason"));
        assert!(!result
            .message
            .error_message()
            .unwrap_or_default()
            .contains("fixture-key"));

        let non_success = PlannedResponse::json(
            401,
            r#"{"error":{"message":"fixture upstream rejected request"}}"#,
        );
        let result = run_stream(&model, &options, vec![non_success]).await;
        assert_eq!(result.message.stop_reason(), Some(StopReason::Error));
        assert!(result
            .message
            .error_message()
            .unwrap_or_default()
            .contains("fixture upstream rejected request"));
        assert!(!result
            .message
            .error_message()
            .unwrap_or_default()
            .contains("fixture-key"));
    }
}

#[tokio::test]
async fn groq_retry_and_abort_are_terminally_deterministic() {
    let model = model_for("groq");
    let retry_options = OpenAIChatOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                max_retries: Some(1),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let mut transient = PlannedResponse::json(429, r#"{"error":{"message":"retry me"}}"#);
    transient.headers.push(("retry-after-ms", "0"));
    let result = run_stream(
        &model,
        &retry_options,
        vec![
            transient,
            PlannedResponse::sse(success_sse(&model.id, false)),
        ],
    )
    .await;
    assert_eq!(result.requests.len(), 2);
    assert_eq!(result.message.stop_reason(), Some(StopReason::Stop));
    assert!(result
        .events
        .iter()
        .any(|event| matches!(event, AssistantMessageEvent::Done { .. })));

    let abort_signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let abort_options = OpenAIChatOptions {
        base: StreamOptions {
            base: ProviderRequestOptions::default(),
            abort_signal: Some(Arc::clone(&abort_signal)),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut delayed = PlannedResponse::sse(success_sse(&model.id, false));
    delayed.delay = Duration::from_millis(200);
    let fixture = spawn_fixture(vec![delayed]).await;
    let stream = openai_completions::stream(
        &model,
        &fixture_context(false),
        reqwest::Client::new(),
        &fixture.base_url,
        Some("fixture-key"),
        &abort_options,
    );
    for _ in 0..100 {
        if fixture.request_seen.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        fixture.request_seen.load(Ordering::Acquire),
        "fixture request was not observed"
    );
    abort_signal.store(true, Ordering::Release);
    let (events, message) = stream.collect().await;
    fixture.server.await.unwrap();
    assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
    assert!(events.iter().any(|event| matches!(
        event,
        AssistantMessageEvent::Error {
            reason: pi_ai::types::ErrorReason::Aborted,
            ..
        }
    )));
    assert_eq!(fixture.captured.lock().unwrap().len(), 1);
}
