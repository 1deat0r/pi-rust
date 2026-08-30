#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Provider-level loopback coverage for Qwen Token Plan endpoint dispatch.

use pi_ai::providers::all::qwen_token_plan_provider;
use pi_ai::types::{
    Context, Message, ProviderRequestOptions, StopReason, StreamOptions, UserContent,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = socket
            .read(&mut chunk)
            .await
            .expect("read loopback request");
        assert!(count > 0, "client closed before completing request");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let header_text = String::from_utf8_lossy(&request[..header_end]);
        let content_length = header_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .unwrap_or(0);
        if request.len() >= header_end + content_length {
            return request;
        }
    }
}

#[tokio::test]
async fn qwen_provider_dispatch_uses_model_base_url() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let address = listener.local_addr().expect("loopback listener address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept loopback request");
        let request = read_request(&mut socket).await;
        let request_text = String::from_utf8_lossy(&request);
        assert!(request_text.starts_with("POST /compatible-mode/v1/chat/completions HTTP/1.1\r\n"));
        assert!(request_text
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-qwen-key"));
        let body = concat!(
            "data: {\"id\":\"qwen-loopback\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"qwen-loopback\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write loopback response");
        request
    });

    let provider = qwen_token_plan_provider();
    let mut model = provider
        .models
        .iter()
        .find(|model| model.id == "qwen3.7-max")
        .cloned()
        .expect("Qwen catalog model");
    model.base_url = format!("http://{address}/compatible-mode/v1");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserContent::string("hello", 1))],
        tools: Vec::new(),
    };
    let options = StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some("synthetic-qwen-key".to_string()),
            max_retries: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    let streams = provider
        .single_streams
        .as_ref()
        .expect("Qwen provider has a single API stream");
    let stream = (streams.stream)(&model, &context, Some(&options));
    let (_events, message) = stream.collect().await;
    let request = server.await.expect("loopback server task");

    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert!(message.error_message().is_none());
    assert!(
        String::from_utf8_lossy(&request).starts_with("POST /compatible-mode/v1/chat/completions")
    );
}
