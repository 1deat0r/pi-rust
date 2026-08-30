#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Real loopback coverage for the Bedrock Converse request/stream boundary.
//!
//! These tests exercise the public adaptor against an actual TCP listener. No
//! provider or Codex turn is mocked: the request is serialized by reqwest,
//! response headers pass through `on_response`, and the binary AWS eventstream
//! is consumed by the Bedrock transport.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pi_ai::api::bedrock_converse::{stream, BedrockOptions};
use pi_ai::model::Model;
use pi_ai::types::{
    AssistantMessageEvent, Context, ErrorReason, OnPayloadFn, ProviderEnv, ProviderRequestOptions,
    ProviderResponse, StopReason, StreamOptions, UserContent,
};

#[derive(Debug)]
struct CapturedRequest {
    headers: std::collections::BTreeMap<String, String>,
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
    let mut headers = std::collections::BTreeMap::new();
    let content_length = header_text
        .lines()
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
        headers,
        body: raw[header_end..header_end + content_length].to_vec(),
    }
}

fn status_line(status: u16) -> &'static str {
    match status {
        200 => "200 OK",
        408 => "408 Request Timeout",
        429 => "429 Too Many Requests",
        500 => "500 Internal Server Error",
        _ => "500 Internal Server Error",
    }
}

async fn write_response(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut header = format!(
        "HTTP/1.1 {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        status_line(status),
        body.len()
    );
    for (name, value) in extra_headers {
        header.push_str(name);
        header.push_str(": ");
        header.push_str(value);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(body).await
}

async fn bind_server() -> (String, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let address = listener.local_addr().expect("loopback listener address");
    (format!("http://{address}"), listener)
}

fn event_frame(event_type: &str, payload: &Value) -> Vec<u8> {
    let mut headers = Vec::new();
    let mut push_header = |name: &str, value: &str| {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(6); // AWS eventstream string value
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    };
    push_header(":message-type", "event");
    push_header(":event-type", event_type);
    let payload = serde_json::to_vec(payload).expect("serialize event payload");
    let total_length = 16 + headers.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total_length);
    frame.extend_from_slice(&[0x00, 0xC0, 0xDE, 0x00]);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

fn successful_eventstream() -> Vec<u8> {
    [
        event_frame(
            "messageStart",
            &json!({"messageStart": {"role": "assistant"}}),
        ),
        event_frame(
            "contentBlockDelta",
            &json!({"contentBlockDelta": {"contentBlockIndex": 0, "delta": {"text": "hello"}}}),
        ),
        event_frame(
            "contentBlockStop",
            &json!({"contentBlockStop": {"contentBlockIndex": 0}}),
        ),
        event_frame(
            "messageStop",
            &json!({"messageStop": {"stopReason": "end_turn"}}),
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn midstream_error_eventstream() -> Vec<u8> {
    [
        event_frame(
            "messageStart",
            &json!({"messageStart": {"role": "assistant"}}),
        ),
        event_frame(
            "throttlingException",
            &json!({"throttlingException": {"message": "slow down"}}),
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn context() -> Context {
    Context {
        messages: vec![pi_ai::types::Message::User(UserContent::string("hello", 1))],
        ..Default::default()
    }
}

fn model(base_url: String) -> Model {
    let mut model = Model::new(
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "Claude Sonnet 4.5",
        "bedrock-converse-stream",
        "amazon-bedrock",
    );
    model.base_url = base_url;
    model
}

fn skip_auth_options() -> BedrockOptions {
    let mut env = ProviderEnv::new();
    env.insert("AWS_BEDROCK_SKIP_AUTH".to_string(), "1".to_string());
    BedrockOptions {
        base: StreamOptions {
            base: ProviderRequestOptions {
                env: Some(env),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn payload_hook_mutates_wire_body_and_on_response_sees_headers() {
    let (base_url, listener) = bind_server().await;
    let (request_tx, request_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let request = read_request(&mut socket).await;
        let _ = request_tx.send(request);
        write_response(
            &mut socket,
            200,
            "application/vnd.amazon.eventstream",
            &[("x-amzn-requestid", "req-payload")],
            &successful_eventstream(),
        )
        .await
        .expect("write response");
    });

    let payload_seen = Arc::new(AtomicBool::new(false));
    let payload_seen_by_hook = payload_seen.clone();
    let hook: OnPayloadFn = Arc::new(move |mut payload, _model| {
        let payload_seen = payload_seen_by_hook.clone();
        Box::pin(async move {
            payload_seen.store(true, Ordering::SeqCst);
            payload
                .as_object_mut()
                .expect("Bedrock command input object")
                .insert("loopbackMarker".to_string(), json!("mutated"));
            Some(payload)
        })
    });
    let responses: Arc<Mutex<Vec<ProviderResponse>>> = Arc::default();
    let responses_by_hook = responses.clone();
    let mut options = skip_auth_options();
    options.base.on_payload = Some(hook);
    options.base.on_response = Some(Arc::new(move |response, _model| {
        responses_by_hook.lock().unwrap().push(response.clone());
    }));

    let (events, message) = tokio::time::timeout(
        Duration::from_secs(2),
        stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &options,
        )
        .collect(),
    )
    .await
    .expect("Bedrock payload stream should settle");
    let request = request_rx.await.expect("captured request");
    task.await.expect("server task");

    assert!(payload_seen.load(Ordering::SeqCst));
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert!(events.iter().any(
        |event| matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "hello")
    ));
    let payload: Value = serde_json::from_slice(&request.body).expect("JSON request body");
    assert_eq!(payload["loopbackMarker"], json!("mutated"));
    assert_eq!(request.headers["content-type"], "application/json");
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].status, 200);
    assert_eq!(responses[0].headers["x-amzn-requestid"], "req-payload");
}

#[tokio::test]
async fn abort_before_request_runs_payload_hook_but_never_opens_socket() {
    let (base_url, listener) = bind_server().await;
    let accepted = tokio::spawn(async move {
        match tokio::time::timeout(Duration::from_millis(250), listener.accept()).await {
            Ok(Ok((socket, _))) => {
                drop(socket);
                true
            }
            Ok(Err(error)) => panic!("accept failed: {error}"),
            Err(_) => false,
        }
    });

    let signal = Arc::new(AtomicBool::new(true));
    let payload_seen = Arc::new(AtomicBool::new(false));
    let payload_seen_by_hook = payload_seen.clone();
    let hook: OnPayloadFn = Arc::new(move |payload, _model| {
        let payload_seen = payload_seen_by_hook.clone();
        Box::pin(async move {
            payload_seen.store(true, Ordering::SeqCst);
            Some(payload)
        })
    });
    let mut options = skip_auth_options();
    options.base.abort_signal = Some(signal);
    options.base.on_payload = Some(hook);

    let (events, message) = tokio::time::timeout(
        Duration::from_secs(1),
        stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &options,
        )
        .collect(),
    )
    .await
    .expect("pre-aborted Bedrock stream should settle");

    assert!(payload_seen.load(Ordering::SeqCst));
    assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
    assert!(message.diagnostics().is_none());
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                ..
            }
        )
    }));
    assert!(!accepted.await.expect("accept watcher"));
}

#[tokio::test]
async fn abort_during_response_body_stops_incremental_eventstream_read() {
    let (base_url, listener) = bind_server().await;
    let (first_chunk_tx, first_chunk_rx) = tokio::sync::oneshot::channel();
    let body = successful_eventstream();
    let split_at = body.len() / 2;
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let _request = read_request(&mut socket).await;
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\nconnection: close\r\nx-amzn-requestid: req-abort\r\n\r\n",
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write response headers");
        socket
            .write_all(&body[..split_at])
            .await
            .expect("write first response chunk");
        socket.flush().await.expect("flush first response chunk");
        let _ = first_chunk_tx.send(());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = socket.write_all(&body[split_at..]).await;
    });

    let signal = Arc::new(AtomicBool::new(false));
    let signal_for_test = signal.clone();
    let mut options = skip_auth_options();
    options.base.abort_signal = Some(signal);
    let started = Instant::now();
    let (events, message) = tokio::time::timeout(Duration::from_secs(2), async move {
        let stream = stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &options,
        );
        first_chunk_rx.await.expect("first response chunk");
        signal_for_test.store(true, Ordering::SeqCst);
        stream.collect().await
    })
    .await
    .expect("mid-body abort should settle");
    let stream_elapsed = started.elapsed();
    task.await.expect("server task");

    assert_eq!(message.stop_reason(), Some(StopReason::Aborted));
    assert_eq!(message.error_message(), Some("Request was aborted"));
    assert!(message.diagnostics().is_none());
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                ..
            }
        )
    }));
    assert!(
        stream_elapsed < Duration::from_millis(450),
        "stream took {stream_elapsed:?}"
    );
}

#[tokio::test]
async fn provider_error_exposes_bounded_diagnostics_and_preserves_on_response() {
    let (base_url, listener) = bind_server().await;
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let _request = read_request(&mut socket).await;
        let body = br#"{"code":"ThrottlingException","message":"slow down"}"#;
        write_response(
            &mut socket,
            429,
            "application/json",
            &[
                ("x-amzn-requestid", "req-throttle"),
                ("x-amzn-errortype", "ThrottlingException:sender"),
            ],
            body,
        )
        .await
        .expect("write response");
    });

    let responses: Arc<Mutex<Vec<ProviderResponse>>> = Arc::default();
    let responses_by_hook = responses.clone();
    let mut options = skip_auth_options();
    options.base.on_response = Some(Arc::new(move |response, _model| {
        responses_by_hook.lock().unwrap().push(response.clone());
    }));
    let (_events, message) = tokio::time::timeout(
        Duration::from_secs(2),
        stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &options,
        )
        .collect(),
    )
    .await
    .expect("provider error stream should settle");
    task.await.expect("server task");

    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert_eq!(
        message.error_message(),
        Some("429: {\"code\":\"ThrottlingException\",\"message\":\"slow down\"}")
    );
    let diagnostic = message
        .diagnostics()
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.diagnostic_type == "bedrock_response_failure")
        })
        .expect("Bedrock failure diagnostic");
    assert_eq!(
        diagnostic.details.as_ref().expect("diagnostic details"),
        &std::collections::BTreeMap::from([
            ("errorCode".to_string(), json!("ThrottlingException")),
            ("requestId".to_string(), json!("req-throttle")),
            ("status".to_string(), json!(429)),
        ])
    );
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].status, 429);
    assert_eq!(responses[0].headers["x-amzn-requestid"], "req-throttle");
    assert_eq!(
        responses[0].headers["x-amzn-errortype"],
        "ThrottlingException:sender"
    );
}

#[tokio::test]
async fn midstream_provider_error_includes_code_and_request_id() {
    let (base_url, listener) = bind_server().await;
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let _request = read_request(&mut socket).await;
        write_response(
            &mut socket,
            200,
            "application/vnd.amazon.eventstream",
            &[("x-amzn-requestid", "req-stream-error")],
            &midstream_error_eventstream(),
        )
        .await
        .expect("write response");
    });

    let (_events, message) = tokio::time::timeout(
        Duration::from_secs(2),
        stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &skip_auth_options(),
        )
        .collect(),
    )
    .await
    .expect("midstream provider error should settle");
    task.await.expect("server task");

    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert_eq!(message.error_message(), Some("Throttling error: slow down"));
    let diagnostic = message
        .diagnostics()
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.diagnostic_type == "bedrock_response_failure")
        })
        .expect("Bedrock failure diagnostic");
    assert_eq!(
        diagnostic.details.as_ref().expect("diagnostic details"),
        &std::collections::BTreeMap::from([
            ("errorCode".to_string(), json!("ThrottlingException")),
            ("requestId".to_string(), json!("req-stream-error")),
        ])
    );
}

#[tokio::test]
async fn overlong_provider_metadata_is_omitted_from_diagnostics() {
    let (base_url, listener) = bind_server().await;
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let _request = read_request(&mut socket).await;
        let code = "E".repeat(201);
        let request_id = "R".repeat(201);
        let error_type = format!("{code}:sender");
        let body = format!(r#"{{"code":"{code}","message":"slow down"}}"#);
        write_response(
            &mut socket,
            429,
            "application/json",
            &[
                ("x-amzn-requestid", request_id.as_str()),
                ("x-amzn-errortype", error_type.as_str()),
            ],
            body.as_bytes(),
        )
        .await
        .expect("write response");
    });

    let (_events, message) = tokio::time::timeout(
        Duration::from_secs(2),
        stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &skip_auth_options(),
        )
        .collect(),
    )
    .await
    .expect("overlong provider error should settle");
    task.await.expect("server task");

    let diagnostic = message
        .diagnostics()
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|diagnostic| diagnostic.diagnostic_type == "bedrock_response_failure")
        })
        .expect("Bedrock failure diagnostic");
    assert_eq!(
        diagnostic.details.as_ref().expect("diagnostic details"),
        &std::collections::BTreeMap::from([("status".to_string(), json!(429))])
    );
}

#[tokio::test]
async fn timeout_option_stops_a_response_that_has_not_started() {
    let (base_url, listener) = bind_server().await;
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let _request = read_request(&mut socket).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = write_response(
            &mut socket,
            200,
            "application/vnd.amazon.eventstream",
            &[],
            &successful_eventstream(),
        )
        .await;
    });

    let mut options = skip_auth_options();
    options.base.base.timeout_ms = Some(40);
    let started = Instant::now();
    let (_events, message) = tokio::time::timeout(
        Duration::from_secs(1),
        stream(
            &model(base_url),
            &context(),
            reqwest::Client::new(),
            None,
            &options,
        )
        .collect(),
    )
    .await
    .expect("timeout should settle");
    let stream_elapsed = started.elapsed();
    task.await.expect("server task");

    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert!(
        message
            .error_message()
            .is_some_and(|message| message.contains("Request failed")),
        "unexpected timeout error: {:?}",
        message.error_message()
    );
    assert!(
        stream_elapsed < Duration::from_millis(180),
        "timeout was not enforced before the delayed response: {stream_elapsed:?}"
    );
}
