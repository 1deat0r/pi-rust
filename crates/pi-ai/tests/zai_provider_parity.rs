#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Offline parity evidence for the pinned Z.AI provider registrations.
//!
//! The model lists and auth labels here mirror the upstream Z.AI provider
//! modules. Inference success/error traffic is covered by the shared local
//! OpenAI-completions matrix; this file keeps the provider-specific catalog
//! and credential precedence assertions deterministic and secret-free.

use std::sync::Arc;

use pi_ai::api::openai_completions::{build_params, OpenAiCompletionsCompat};
use pi_ai::auth::{ApiKeyCredential, AuthContext};
use pi_ai::model::ModelInput;
use pi_ai::models::Provider;
use pi_ai::providers::{zai_coding_cn_provider, zai_provider};
use pi_ai::types::{
    Context, Message, ProviderRequestOptions, StopReason, StreamOptions, UserContent,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = socket.read(&mut chunk).await.expect("read fixture request");
        assert!(count > 0, "fixture client closed before request");
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("valid content length"))
            })
            .unwrap_or(0);
        if request.len() >= header_end + length {
            return request;
        }
    }
}

type ProviderCase = (
    &'static str,
    fn() -> Provider,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
);

fn fixture_context(name: &'static str, value: &'static str) -> AuthContext {
    let name = name.to_string();
    let value = value.to_string();
    AuthContext {
        env: Arc::new(move |candidate| (candidate == name.as_str()).then(|| value.clone())),
        file_exists: Arc::new(|_| false),
    }
}

fn model_ids(provider: &Provider) -> Vec<&str> {
    provider
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect()
}

#[test]
fn zai_registrations_match_pinned_catalog_and_auth_contract() {
    let cases: &[ProviderCase] = &[
        (
            "zai",
            zai_provider,
            "Z.AI",
            "Z.AI API key",
            "ZAI_API_KEY",
            "https://api.z.ai/api/paas/v4",
            &[
                "glm-4.7",
                "glm-5-turbo",
                "glm-5.2",
                "glm-5.2-highspeed",
                "glm-5.3",
                "glm-5.3-flash",
                "glm-5.3-highspeed",
            ],
        ),
        (
            "zai-coding-cn",
            zai_coding_cn_provider,
            "Z.AI Coding CN",
            "Z.AI Coding CN API key",
            "ZAI_CODING_CN_API_KEY",
            "https://open.bigmodel.cn/api/coding/paas/v4",
            &[
                "glm-4.6v",
                "glm-4.7",
                "glm-5-turbo",
                "glm-5.1",
                "glm-5.2",
                "glm-5.2-highspeed",
                "glm-5.3",
                "glm-5.3-flash",
                "glm-5.3-highspeed",
                "glm-5v-turbo",
            ],
        ),
    ];

    for &(id, constructor, name, auth_name, env_name, base_url, expected_ids) in cases {
        let provider = constructor();
        assert_eq!(provider.id, id);
        assert_eq!(provider.name, name);
        assert_eq!(provider.base_url.as_deref(), Some(base_url));
        assert_eq!(model_ids(&provider).as_slice(), expected_ids);
        assert!(provider
            .models
            .iter()
            .all(|model| model.api == "openai-completions"));
        assert!(provider
            .models
            .iter()
            .all(|model| model.provider == id && model.base_url == base_url));

        let auth = provider.auth.api_key.expect("Z.AI API-key auth");
        assert_eq!(auth.name(), auth_name);

        let context = fixture_context(env_name, "fixture-zai-key");
        let checked = auth.check(&context, None).expect("fixture env key check");
        assert_eq!(checked.source.as_deref(), Some(env_name));
        let resolved = auth.resolve(&context, None).expect("fixture env key");
        assert_eq!(resolved.source.as_deref(), Some(env_name));
        assert_eq!(resolved.auth.api_key.as_deref(), Some("fixture-zai-key"));

        let stored = ApiKeyCredential {
            key: Some("stored-zai-key".to_string()),
            env: None,
        };
        let stored_result = auth
            .resolve(&context, Some(&stored))
            .expect("stored key takes precedence");
        assert_eq!(stored_result.source.as_deref(), Some("stored credential"));
        assert_eq!(
            stored_result.auth.api_key.as_deref(),
            Some("stored-zai-key")
        );

        let empty = fixture_context(env_name, "  ");
        assert!(auth.check(&empty, None).is_none());
        assert!(auth.resolve(&empty, None).is_none());

        let unrelated = fixture_context("UNRELATED_PROVIDER_KEY", "other-key");
        assert!(auth.resolve(&unrelated, None).is_none());
    }
}

#[test]
fn zai_catalog_preserves_reasoning_tool_stream_and_image_dimensions() {
    let standard = zai_provider();
    let glm53_flash = standard
        .models
        .iter()
        .find(|model| model.id == "glm-5.3-flash")
        .expect("Z.AI GLM-5.3 Flash");
    assert_eq!(glm53_flash.input, vec![ModelInput::Text, ModelInput::Image]);
    assert_eq!(glm53_flash.cost.input, 0.075);
    assert_eq!(glm53_flash.cost.output, 0.25);
    assert_eq!(glm53_flash.cost.cache_read, 0.015);
    assert_eq!(glm53_flash.context_window, 1_000_000);
    assert_eq!(glm53_flash.max_tokens, 131_072);
    assert_eq!(
        glm53_flash
            .compat
            .as_ref()
            .and_then(|compat| compat.get("zaiToolStream"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let glm52 = standard
        .models
        .iter()
        .find(|model| model.id == "glm-5.2")
        .expect("Z.AI GLM-5.2");
    assert_eq!(
        glm52
            .thinking_level_map
            .as_ref()
            .and_then(|levels| levels.get(&pi_ai::types::ModelThinkingLevel::Off)),
        Some(&Some("none".to_string()))
    );
    assert_eq!(
        glm52
            .compat
            .as_ref()
            .and_then(|compat| compat.get("supportsReasoningEffort"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let china = zai_coding_cn_provider();
    for model_id in ["glm-4.6v", "glm-5v-turbo"] {
        let model = china
            .models
            .iter()
            .find(|model| model.id == model_id)
            .unwrap_or_else(|| panic!("missing Z.AI Coding CN model {model_id}"));
        assert_eq!(model.input, vec![ModelInput::Text, ModelInput::Image]);
        assert_eq!(
            model
                .compat
                .as_ref()
                .and_then(|compat| compat.get("zaiToolStream"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }
}

#[test]
fn zai_completions_wire_shape_preserves_reasoning_tools_and_max_tokens() {
    for (provider_id, model_id) in [("zai", "glm-5.2"), ("zai-coding-cn", "glm-5.2")] {
        let provider = if provider_id == "zai" {
            zai_provider()
        } else {
            zai_coding_cn_provider()
        };
        let model = provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .expect("GLM-5.2 catalog model");
        let compat = OpenAiCompletionsCompat::get(model);
        let context = Context {
            tools: vec![pi_ai::types::json_tool(
                "ping",
                "Ping the fixture",
                &json!({"type":"object","properties":{}}),
            )],
            ..Default::default()
        };
        let options = StreamOptions {
            max_tokens: Some(123),
            sampling_params: Some(json!({"reasoningEffort":"high"})),
            ..Default::default()
        };
        let params = build_params(model, &context, Some(&options), &compat, "none")
            .expect("Z.AI request parameters");

        assert_eq!(params["max_tokens"], json!(123), "{provider_id}");
        assert!(
            params.get("max_completion_tokens").is_none(),
            "{provider_id}"
        );
        assert_eq!(
            params["thinking"],
            json!({"type":"enabled","clear_thinking":false}),
            "{provider_id}"
        );
        assert_eq!(params["reasoning_effort"], json!("high"), "{provider_id}");
        assert_eq!(params["tool_stream"], json!(true), "{provider_id}");
        assert_eq!(params["tools"][0]["function"]["name"], json!("ping"));

        let off = StreamOptions {
            max_tokens: Some(123),
            ..Default::default()
        };
        let off_params = build_params(model, &context, Some(&off), &compat, "none")
            .expect("Z.AI non-reasoning request parameters");
        assert_eq!(off_params["thinking"], json!({"type":"disabled"}));
        assert!(off_params.get("reasoning_effort").is_none());
        assert_eq!(off_params["tool_stream"], json!(true));

        let unsupported = StreamOptions {
            sampling_params: Some(json!({"reasoningEffort":"medium"})),
            ..Default::default()
        };
        let unsupported_params = build_params(model, &context, Some(&unsupported), &compat, "none")
            .expect("Z.AI unsupported-effort request parameters");
        assert_eq!(
            unsupported_params["thinking"],
            json!({"type":"enabled","clear_thinking":false}),
            "{provider_id}"
        );
        assert!(
            unsupported_params.get("reasoning_effort").is_none(),
            "{provider_id}"
        );
    }
}

#[tokio::test]
async fn zai_provider_uses_general_endpoint_and_bearer_auth_for_streaming() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Z.AI fixture");
    let address = listener.local_addr().expect("fixture address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept Z.AI request");
        let request = read_http_request(&mut socket).await;
        let request_text = String::from_utf8_lossy(&request);
        assert!(request_text.starts_with("POST /api/paas/v4/chat/completions HTTP/1.1\r\n"));
        assert!(request_text
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-zai-key"));
        let body = concat!(
            "data: {\"id\":\"zai-fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"zai-fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
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
            .expect("write Z.AI fixture response");
        request
    });

    let provider = zai_provider();
    let mut model = provider
        .models
        .iter()
        .find(|model| model.id == "glm-5.2")
        .cloned()
        .expect("Z.AI catalog model");
    model.base_url = format!("http://{address}/api/paas/v4");
    let context = Context {
        messages: vec![Message::User(UserContent::string("hello", 1))],
        ..Default::default()
    };
    let options = StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some("fixture-zai-key".to_string()),
            max_retries: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    let streams = provider.single_streams.as_ref().expect("Z.AI stream");
    let message = (streams.stream)(&model, &context, Some(&options))
        .collect()
        .await
        .1;
    let _request = server.await.expect("Z.AI fixture task");
    assert_eq!(message.stop_reason(), Some(StopReason::Stop));
    assert!(matches!(
        message.content().first(),
        Some(pi_ai::types::ContentBlock::Text { text, .. }) if text == "ok"
    ));
}
