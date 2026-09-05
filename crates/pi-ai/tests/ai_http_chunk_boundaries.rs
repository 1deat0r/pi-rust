#![allow(clippy::expect_used, clippy::panic)]

use std::time::Duration;

use pi_ai::api::openai_completions::{self, OpenAIChatOptions};
use pi_ai::types::{
    Context, Message, ProviderRequestOptions, StopReason, StreamOptions, UserContent,
};
use pi_ai::Model;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn spawn_raw_http(chunks: Vec<Vec<u8>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("loopback address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let count = socket.read(&mut buffer).await.expect("read request");
            assert!(count > 0, "client closed before request headers");
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        for chunk in chunks {
            socket
                .write_all(&chunk)
                .await
                .expect("write response chunk");
            tokio::task::yield_now().await;
        }
        socket.shutdown().await.expect("close response");
    });
    format!("http://{address}")
}

fn context() -> Context {
    Context {
        messages: vec![Message::User(UserContent::string("hello", 1))],
        ..Default::default()
    }
}

fn stream(base_url: &str) -> pi_ai::event_stream::AssistantMessageEventStream {
    let mut model = Model::new(
        "chunk-fixture",
        "chunk-fixture",
        "openai-completions",
        "openai",
    );
    model.base_url = base_url.to_string();
    openai_completions::stream(
        &model,
        &context(),
        reqwest::Client::new(),
        base_url,
        Some("synthetic-key"),
        &OpenAIChatOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    timeout_ms: Some(2_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

fn sse_body() -> Vec<u8> {
    concat!(
        "data: {\"id\":\"chunked\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"héllo 世界\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .as_bytes()
    .to_vec()
}

fn message_text(message: &pi_ai::AssistantMessage) -> String {
    message
        .content()
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn bytewise_content_length_response_preserves_partial_utf8_and_sse_frames() {
    let body = sse_body();
    let response = [
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes(),
        body,
    ]
    .concat();
    let base_url = spawn_raw_http(response.into_iter().map(|byte| vec![byte]).collect()).await;

    let (_, message) = tokio::time::timeout(Duration::from_secs(3), stream(&base_url).collect())
        .await
        .expect("bytewise response should settle");
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(message_text(&message), "héllo 世界");
}

#[tokio::test(flavor = "current_thread")]
async fn one_byte_http_chunks_preserve_partial_utf8_and_sse_frames() {
    let mut response = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
    for byte in sse_body() {
        response.extend_from_slice(b"1\r\n");
        response.push(byte);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"0\r\n\r\n");
    let base_url = spawn_raw_http(response.into_iter().map(|byte| vec![byte]).collect()).await;

    let (_, message) = tokio::time::timeout(Duration::from_secs(3), stream(&base_url).collect())
        .await
        .expect("chunked response should settle");
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert_eq!(message_text(&message), "héllo 世界");
}

#[tokio::test(flavor = "current_thread")]
async fn truncated_content_length_and_malformed_chunk_framing_fail_once() {
    let body = sse_body();
    let truncated = [
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len() + 7
        )
        .into_bytes(),
        body,
    ]
    .concat();
    let base_url = spawn_raw_http(vec![truncated]).await;
    let (events, message) =
        tokio::time::timeout(Duration::from_secs(3), stream(&base_url).collect())
            .await
            .expect("truncated response should settle");
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, pi_ai::AssistantMessageEvent::Error { .. }))
            .count(),
        1
    );

    let malformed = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\nZZ\r\ndata: {}\n\n\r\n0\r\n\r\n".to_vec();
    let base_url = spawn_raw_http(vec![malformed]).await;
    let (events, message) =
        tokio::time::timeout(Duration::from_secs(3), stream(&base_url).collect())
            .await
            .expect("malformed chunk response should settle");
    assert_eq!(message.stop_reason(), Some(StopReason::Error));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, pi_ai::AssistantMessageEvent::Error { .. }))
            .count(),
        1
    );
}
