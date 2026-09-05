#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // integration assertions intentionally fail loudly

use pi_ai::types::{
    Context, Message, ProviderRequestOptions, StopReason, StreamOptions, UserContent,
};
use pi_coding_agent::core::model_config::ModelConfig;
use pi_coding_agent::core::model_registry::ModelRegistry;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = socket
            .read(&mut chunk)
            .await
            .expect("read loopback request");
        assert!(count > 0, "client closed before request completed");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        if request.len() >= header_end + content_length {
            return request;
        }
    }
}

#[tokio::test]
async fn models_json_only_provider_authenticates_and_streams_through_native_api() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let address = listener.local_addr().expect("loopback address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let request = read_http_request(&mut socket).await;
        let request_text = String::from_utf8_lossy(&request);
        assert!(request_text.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        let lowercase = request_text.to_ascii_lowercase();
        assert!(lowercase.contains("authorization: bearer synthetic-custom-key"));
        assert!(lowercase.contains("x-models-json: runtime-proof"));
        let body = concat!(
            "data: {\"id\":\"custom-loopback\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"custom ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"custom-loopback\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
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
            .expect("write response");
        request
    });

    let config = ModelConfig::from_value(json!({
        "providers": {
            "local-custom": {
                "name": "Local custom",
                "baseUrl": format!("http://{address}/v1"),
                "api": "openai-completions",
                "apiKey": "synthetic-custom-key",
                "authHeader": true,
                "headers": { "X-Models-Json": "runtime-proof" },
                "models": [{ "id": "local-model", "name": "Local model" }]
            }
        }
    }))
    .expect("valid models.json config");
    let registry = ModelRegistry::new(
        pi_ai::models::create_models(pi_ai::models::CreateModelsOptions::default()),
        config,
    );
    let models = registry.into_models();
    let model = models
        .get_model("local-custom", "local-model")
        .expect("custom runtime model");
    let context = Context {
        system_prompt: None,
        messages: vec![Message::User(UserContent::string("hello", 1))],
        tools: Vec::new(),
    };
    let options = StreamOptions {
        base: ProviderRequestOptions {
            max_retries: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    let (_events, message) = models
        .stream(&model, &context, Some(&options))
        .collect()
        .await;
    let request = server.await.expect("loopback server");

    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert!(message.error_message().is_none());
    assert!(String::from_utf8_lossy(&request).contains("local-model"));
}
