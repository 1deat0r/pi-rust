#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Deterministic wire fixtures for Anthropic OAuth/provider edge parity.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_ai::api::anthropic_messages::{build_params, process_anthropic_events, AnthropicOptions};
use pi_ai::model::{Model, ModelCost};
use pi_ai::sse::SseParser;
use pi_ai::types::{
    AssistantMessage, ContentBlock, Context, Message, SimpleStreamOptions, StopReason,
    StreamOptions, ThinkingLevel, Tool, ToolResultMessage, UserContent,
};

#[derive(Debug)]
struct CapturedRequest {
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
}

fn model(id: &str, reasoning: bool) -> Model {
    let mut model = Model::new(id, id, "anthropic-messages", "anthropic");
    model.reasoning = reasoning;
    model.base_url = "http://127.0.0.1".to_string();
    model.max_tokens = 16_384;
    model.cost = ModelCost {
        input: 1.0,
        output: 2.0,
        cache_read: 0.1,
        cache_write: 0.5,
        tiers: None,
    };
    if reasoning {
        model.thinking_level_map = Some(BTreeMap::from([
            (
                pi_ai::types::ModelThinkingLevel::Off,
                Some("off".to_string()),
            ),
            (
                pi_ai::types::ModelThinkingLevel::Minimal,
                Some("low".to_string()),
            ),
            (
                pi_ai::types::ModelThinkingLevel::Low,
                Some("low".to_string()),
            ),
            (
                pi_ai::types::ModelThinkingLevel::Medium,
                Some("medium".to_string()),
            ),
            (
                pi_ai::types::ModelThinkingLevel::High,
                Some("high".to_string()),
            ),
            (
                pi_ai::types::ModelThinkingLevel::Xhigh,
                Some("xhigh".to_string()),
            ),
            (
                pi_ai::types::ModelThinkingLevel::Max,
                Some("max".to_string()),
            ),
        ]));
    }
    model
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: format!("The {name} tool"),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
        }),
        constrained_sampling: None,
    }
}

fn assistant_tool_call(name: &str, id: &str) -> AssistantMessage {
    let mut message = AssistantMessage::new();
    message.set_api_provider_model("anthropic-messages", "anthropic", "claude-opus-4-6");
    message.content_mut().push(ContentBlock::tool_call(
        id,
        name,
        serde_json::json!({"path": "x"}),
    ));
    message.set_stop_reason(StopReason::ToolUse);
    message
}

fn tool_result(id: &str, name: &str, added: &[&str]) -> Message {
    let mut result = ToolResultMessage::text(id, name, "done", false);
    match &mut result {
        ToolResultMessage::ToolResult {
            added_tool_names, ..
        } => {
            if !added.is_empty() {
                *added_tool_names = Some(added.iter().map(|name| (*name).to_string()).collect());
            }
        }
    }
    Message::ToolResult(result)
}

fn minimal_response(model: &str) -> &'static str {
    let _ = model;
    "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n"
}

async fn run_fixture(
    model: &Model,
    context: &Context,
    api_key: Option<&str>,
    options: &AnthropicOptions,
    response: &str,
    client: reqwest::Client,
) -> (
    CapturedRequest,
    Vec<pi_ai::types::AssistantMessageEvent>,
    AssistantMessage,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let captured_server = captured.clone();
    let response = response.to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end;
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "fixture client closed before headers");
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = end + 4;
                break;
            }
        }
        let headers_text = String::from_utf8_lossy(&request[..header_end]).into_owned();
        let content_length = headers_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.eq_ignore_ascii_case("content-length"))
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "fixture client closed before body");
            request.extend_from_slice(&buffer[..count]);
        }
        let mut headers = BTreeMap::new();
        for line in headers_text.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let body =
            serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
        *captured_server.lock().unwrap() = Some(CapturedRequest { headers, body });

        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
    });

    let base_url = format!("http://{address}");
    let stream =
        pi_ai::api::anthropic_messages::stream(model, context, client, &base_url, api_key, options);
    let (events, message) = stream.collect().await;
    server.await.unwrap();
    let captured_request = captured
        .lock()
        .unwrap()
        .take()
        .expect("fixture captured request");
    (captured_request, events, message)
}

#[tokio::test]
async fn oauth_wire_headers_and_provider_tool_name_mapping() {
    let model = model("claude-opus-4-6", false);
    let context = Context {
        system_prompt: Some("system prompt".to_string()),
        messages: vec![
            Message::User(UserContent::string("read x", 1)),
            Message::Assistant(assistant_tool_call("read", "call_1")),
            tool_result("call_1", "read", &[]),
        ],
        tools: vec![tool("read")],
    };
    let options = AnthropicOptions {
        base: StreamOptions {
            cache_retention: Some("none".to_string()),
            ..Default::default()
        },
        interleaved_thinking: Some(false),
        ..Default::default()
    };
    let response = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_oauth\",\"model\":\"claude-opus-4-6\",\"usage\":{}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_2\",\"name\":\"Read\",\"input\":{\"path\":\"x\"}}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let client = reqwest::Client::builder()
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::HeaderName::from_static("x-client-fixture"),
            reqwest::header::HeaderValue::from_static("present"),
        )]))
        .build()
        .unwrap();
    let (request, _, message) = run_fixture(
        &model,
        &context,
        Some("sk-ant-oat-fake"),
        &options,
        response,
        client,
    )
    .await;

    assert_eq!(
        request.headers.get("authorization"),
        Some(&"Bearer sk-ant-oat-fake".to_string())
    );
    assert!(!request.headers.contains_key("x-api-key"));
    assert_eq!(
        request.headers.get("anthropic-beta"),
        Some(&"claude-code-20250219,oauth-2025-04-20".to_string())
    );
    assert_eq!(
        request.headers.get("user-agent"),
        Some(&"claude-cli/2.1.75".to_string())
    );
    assert_eq!(request.headers.get("x-app"), Some(&"cli".to_string()));
    assert_eq!(
        request.headers.get("x-client-fixture"),
        Some(&"present".to_string())
    );
    assert_eq!(
        request
            .headers
            .get("anthropic-dangerous-direct-browser-access"),
        Some(&"true".to_string())
    );
    assert_eq!(
        request.body["system"][0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(request.body["system"][1]["text"], "system prompt");
    assert_eq!(request.body["tools"][0]["name"], "Read");
    assert_eq!(request.body["messages"][1]["content"][0]["name"], "Read");
    assert_eq!(message.provider(), Some("anthropic"));
    assert!(matches!(
        &message.content()[0],
        ContentBlock::ToolCall { name, arguments, .. }
            if name == "read" && arguments == &serde_json::json!({"path": "x"})
    ));
}

#[tokio::test]
async fn adaptive_thinking_replays_signed_empty_thinking_and_skips_interleaved_beta() {
    let mut model = model("claude-opus-4-8", true);
    model.compat = Some(serde_json::json!({
        "forceAdaptiveThinking": true,
        "supportsToolReferences": true,
    }));
    let mut assistant = AssistantMessage::new();
    assistant.set_api_provider_model("anthropic-messages", "anthropic", &model.id);
    assistant.content_mut().push(ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: Some("sig-replay".to_string()),
        redacted: None,
    });
    let context = Context {
        messages: vec![
            Message::User(UserContent::string("think", 1)),
            Message::Assistant(assistant),
        ],
        ..Default::default()
    };
    let options = AnthropicOptions {
        thinking_enabled: Some(true),
        effort: Some("medium".to_string()),
        interleaved_thinking: Some(true),
        base: StreamOptions {
            cache_retention: Some("none".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let params = build_params(&model, &context, &options).unwrap();
    assert_eq!(
        params["thinking"],
        serde_json::json!({
            "type": "adaptive",
            "display": "summarized",
        })
    );
    assert_eq!(params["output_config"]["effort"], "medium");
    assert_eq!(params["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(
        params["messages"][1]["content"][0]["signature"],
        "sig-replay"
    );

    let (request, _, _) = run_fixture(
        &model,
        &context,
        Some("api-key"),
        &options,
        minimal_response(&model.id),
        reqwest::Client::new(),
    )
    .await;
    assert!(!request.headers.contains_key("anthropic-beta"));
}

#[tokio::test]
async fn eager_tool_input_controls_beta_and_client_headers() {
    let context = Context {
        messages: vec![Message::User(UserContent::string("use tool", 1))],
        tools: vec![tool("read")],
        ..Default::default()
    };
    let mut legacy_model = model("claude-sonnet-4-6", false);
    legacy_model.compat = Some(serde_json::json!({
        "supportsEagerToolInputStreaming": false,
        "supportsToolReferences": false,
    }));
    let options = AnthropicOptions {
        interleaved_thinking: Some(false),
        base: StreamOptions {
            cache_retention: Some("none".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let (request, _, _) = run_fixture(
        &legacy_model,
        &context,
        Some("api-key"),
        &options,
        minimal_response(&legacy_model.id),
        reqwest::Client::new(),
    )
    .await;
    assert_eq!(
        request.headers.get("anthropic-beta"),
        Some(&"fine-grained-tool-streaming-2025-05-14".to_string())
    );
    assert_eq!(
        request.body["tools"][0]["eager_input_streaming"],
        serde_json::Value::Null
    );

    let mut eager_model = model("claude-sonnet-4-6", false);
    eager_model.compat = Some(serde_json::json!({
        "supportsEagerToolInputStreaming": true,
        "supportsToolReferences": false,
    }));
    let (request, _, _) = run_fixture(
        &eager_model,
        &context,
        Some("api-key"),
        &options,
        minimal_response(&eager_model.id),
        reqwest::Client::new(),
    )
    .await;
    assert!(!request.headers.contains_key("anthropic-beta"));
    assert_eq!(request.body["tools"][0]["eager_input_streaming"], true);
}

#[test]
fn deferred_references_and_server_fallback_preserve_wire_shape() {
    let mut model = model("claude-opus-4-6", false);
    model.compat = Some(serde_json::json!({"supportsToolReferences": true}));
    let context = Context {
        system_prompt: None,
        messages: vec![
            Message::User(UserContent::string("find", 1)),
            Message::Assistant(assistant_tool_call("base_tool", "call_1")),
            tool_result("call_1", "base_tool", &["late_tool"]),
            Message::User(UserContent::string("continue", 4)),
        ],
        tools: vec![tool("base_tool"), tool("late_tool")],
    };
    let params = build_params(&model, &context, &Default::default()).unwrap();
    assert_eq!(params["tools"][0]["name"], "base_tool");
    assert_eq!(params["tools"][1]["name"], "late_tool");
    assert_eq!(params["tools"][1]["defer_loading"], true);
    assert_eq!(
        params["messages"][2]["content"][0]["content"],
        serde_json::json!([{"type": "tool_reference", "tool_name": "late_tool"}])
    );

    model.compat = Some(serde_json::json!({"supportsToolReferences": false}));
    let params = build_params(&model, &context, &Default::default()).unwrap();
    assert_eq!(params["tools"][1]["defer_loading"], serde_json::Value::Null);
    assert_ne!(
        params["messages"][2]["content"][0]["content"][0]["type"],
        "tool_reference"
    );
}

#[test]
fn unspecified_reasoning_does_not_enable_anthropic_thinking() {
    let model = model("MiniMax-M2.7", true);
    let context = Context {
        messages: vec![Message::User(UserContent::string("hello", 1))],
        ..Default::default()
    };

    let omitted = build_params(&model, &context, &Default::default()).unwrap();
    assert!(omitted.get("thinking").is_none());

    let enabled = build_params(
        &model,
        &context,
        &AnthropicOptions {
            thinking_enabled: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        enabled["thinking"],
        serde_json::json!({
            "type": "enabled",
            "budget_tokens": 1024,
            "display": "summarized",
        })
    );
}

#[tokio::test]
async fn server_side_fallback_maps_response_model_and_cost() {
    let mut model = model("claude-opus-4-6", false);
    model.compat = Some(serde_json::json!({
        "allowedFallbackModels": [{
            "provider": "anthropic",
            "model": "claude-sonnet-4-6",
            "cost": {"input": 10.0, "output": 20.0, "cacheRead": 1.0, "cacheWrite": 2.0}
        }]
    }));
    let context = Context {
        messages: vec![Message::User(UserContent::string("hello", 1))],
        ..Default::default()
    };
    let options = AnthropicOptions {
        interleaved_thinking: Some(false),
        base: StreamOptions {
            cache_retention: Some("none".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let params = build_params(&model, &context, &options).unwrap();
    assert_eq!(
        params["fallbacks"],
        serde_json::json!([{"model": "claude-sonnet-4-6"}])
    );
    let response = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fallback\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (request, _, message) = run_fixture(
        &model,
        &context,
        Some("api-key"),
        &options,
        response,
        reqwest::Client::new(),
    )
    .await;
    assert_eq!(
        request.headers.get("anthropic-beta"),
        Some(&"server-side-fallback-2026-07-01".to_string())
    );
    assert_eq!(message.response_id(), Some("msg_fallback"));
    assert_eq!(
        serde_json::to_value(&message).unwrap()["responseModel"],
        "claude-sonnet-4-6"
    );
    assert!((message.usage().unwrap().cost.input - 0.001).abs() < 1e-12);
}

#[test]
fn malformed_and_truncated_known_events_keep_upstream_errors() {
    let model = model("claude-opus-4-6", false);
    let malformed = vec![pi_ai::sse::SseEvent {
        data: "{".to_string(),
        event: Some("message_start".to_string()),
        id: None,
    }];
    let error = process_anthropic_events(&model, &malformed, |_| {}).unwrap_err();
    assert!(error.starts_with("Could not parse Anthropic SSE event message_start:"));

    let events = SseParser::parse_text(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude-opus-4-6\",\"usage\":{}}}\n\n\
         event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
    );
    assert_eq!(
        process_anthropic_events(&model, &events, |_| {}).unwrap_err(),
        "Anthropic stream ended before message_stop"
    );
}

#[test]
fn kimi_tool_stream_repairs_malformed_event_and_final_arguments() {
    let mut model = model("k3", true);
    model.provider = "kimi-coding".to_string();
    let events = vec![
        pi_ai::sse::SseEvent {
            data: r#"{"type":"message_start","message":{"id":"m","model":"k3","usage":{}}}"#.to_string(),
            event: Some("message_start".to_string()),
            id: None,
        },
        pi_ai::sse::SseEvent {
            data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu","name":"edit","input":{}}}"#.to_string(),
            event: Some("content_block_start".to_string()),
            id: None,
        },
        pi_ai::sse::SseEvent {
            data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#.to_string(),
            event: Some("content_block_delta".to_string()),
            id: None,
        },
        pi_ai::sse::SseEvent {
            data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            event: Some("content_block_stop".to_string()),
            id: None,
        },
        pi_ai::sse::SseEvent {
            data: r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#.to_string(),
            event: Some("message_delta".to_string()),
            id: None,
        },
        pi_ai::sse::SseEvent {
            data: r#"{"type":"message_stop"}"#.to_string(),
            event: Some("message_stop".to_string()),
            id: None,
        },
    ];
    let output = process_anthropic_events(&model, &events, |_| {}).unwrap();
    assert_eq!(
        output.content().iter().find_map(|block| match block {
            ContentBlock::ToolCall { arguments, .. } => Some(arguments),
            _ => None,
        }),
        Some(&serde_json::json!({"path": "A\\H", "text": "col1\tcol2"}))
    );
}

#[test]
fn simple_options_type_remains_provider_neutral() {
    let options = SimpleStreamOptions {
        reasoning: Some(ThinkingLevel::Medium),
        ..Default::default()
    };
    assert_eq!(options.reasoning, Some(ThinkingLevel::Medium));
}
