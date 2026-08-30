#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Offline provider/API matrix evidence for the catalog-backed adapters.
//!
//! Each text variant is exercised through the public API adapter against a
//! one-shot local HTTP server.  The test therefore proves request shape,
//! streamed text, usage, response identity, and error encoding without
//! requiring credentials or inventing live-provider evidence.  Constructor
//! dispatch gaps are recorded in the fixture index and covered separately by
//! the no-API controls.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use base64::Engine;
use pi_ai::api::{
    anthropic_messages, azure_openai_responses, bedrock_converse, google_generative_ai,
    google_vertex, mistral_conversations, openai_codex_responses, openai_completions,
    openai_responses, openrouter_images,
};
use pi_ai::images::{self, ImagesOptions};
use pi_ai::model::Model;
use pi_ai::providers::all::builtin_providers;
use pi_ai::types::{
    AssistantMessage, ContentBlock, Context, ImagesContext, ImagesStopReason, Message,
    ProviderRequestOptions, SimpleStreamOptions, StopReason, StreamOptions, ThinkingLevel, Usage,
    UserContent,
};
use serde::Deserialize;
use serde_json::Value;

const MATRIX_INDEX: &str = include_str!("fixtures/provider-matrix/index.json");
const NO_API_FIXTURE: &str = include_str!("fixtures/provider-matrix/no-api.json");

#[derive(Debug, Clone, Deserialize)]
struct MatrixIndex {
    schema_version: u32,
    variants: Vec<MatrixVariant>,
}

#[derive(Debug, Clone, Deserialize)]
struct MatrixVariant {
    provider: String,
    api: String,
    kind: String,
    fixture: String,
    upstream_oracle: Vec<String>,
    evidence_tier: String,
    constructor_status: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiFixture {
    api: String,
    model_field: Option<String>,
    path_fragment: String,
    success: SuccessFixture,
    error: ErrorFixture,
}

#[derive(Debug, Clone, Deserialize)]
struct SuccessFixture {
    content_type: String,
    body: Option<Value>,
    sse: Option<Vec<SseFixture>>,
    frames: Option<Vec<EventStreamFrameFixture>>,
    text: String,
    response_id: Option<String>,
    usage: UsageFixture,
}

#[derive(Debug, Clone, Deserialize)]
struct SseFixture {
    event: Option<String>,
    data: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct EventStreamFrameFixture {
    event_type: String,
    payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorFixture {
    status: u16,
    content_type: String,
    body: Value,
    contains: String,
}

#[derive(Debug, Clone, Deserialize)]
struct UsageFixture {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    total_tokens: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct NoApiFixture {
    evidence_tier: String,
    controls: Vec<NoApiControl>,
}

#[derive(Debug, Clone, Deserialize)]
struct NoApiControl {
    provider: String,
    api: String,
    expected: String,
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn parse_index() -> MatrixIndex {
    serde_json::from_str(MATRIX_INDEX).expect("provider matrix index is valid JSON")
}

fn parse_no_api_fixture() -> NoApiFixture {
    serde_json::from_str(NO_API_FIXTURE).expect("no-api fixture is valid JSON")
}

fn load_api_fixture(name: &str) -> ApiFixture {
    let contents = match name {
        "openai-completions.json" => {
            include_str!("fixtures/provider-matrix/openai-completions.json")
        }
        "openai-responses.json" => include_str!("fixtures/provider-matrix/openai-responses.json"),
        "anthropic-messages.json" => {
            include_str!("fixtures/provider-matrix/anthropic-messages.json")
        }
        "google-generative-ai.json" => {
            include_str!("fixtures/provider-matrix/google-generative-ai.json")
        }
        "google-vertex.json" => include_str!("fixtures/provider-matrix/google-vertex.json"),
        "azure-openai-responses.json" => {
            include_str!("fixtures/provider-matrix/azure-openai-responses.json")
        }
        "openai-codex-responses.json" => {
            include_str!("fixtures/provider-matrix/openai-codex-responses.json")
        }
        "mistral-conversations.json" => {
            include_str!("fixtures/provider-matrix/mistral-conversations.json")
        }
        "bedrock-converse-stream.json" => {
            include_str!("fixtures/provider-matrix/bedrock-converse-stream.json")
        }
        "openrouter-images.json" => {
            include_str!("fixtures/provider-matrix/openrouter-images.json")
        }
        other => panic!("unknown provider matrix fixture {other}"),
    };
    serde_json::from_str(contents).expect("provider API fixture is valid JSON")
}

fn context() -> Context {
    Context {
        messages: vec![Message::User(UserContent::string("Say hello", 1))],
        ..Default::default()
    }
}

fn stream_options(api_key: &str) -> StreamOptions {
    StreamOptions {
        base: ProviderRequestOptions {
            api_key: Some(api_key.to_string()),
            max_retries: Some(0),
            ..Default::default()
        },
        max_tokens: Some(32),
        ..Default::default()
    }
}

fn codex_token() -> String {
    let payload = base64::engine::general_purpose::STANDARD
        .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_test"}}"#);
    format!("aaa.{payload}.bbb")
}

fn invoke_text_stream(
    api: &str,
    model: &Model,
    context: &Context,
    client: reqwest::Client,
    base_url: &str,
    api_key: &str,
    options: &StreamOptions,
) -> pi_ai::AssistantMessageEventStream {
    match api {
        "openai-completions" => openai_completions::stream(
            model,
            context,
            client,
            base_url,
            Some(api_key),
            &openai_completions::OpenAIChatOptions {
                base: options.clone(),
                ..Default::default()
            },
        ),
        "openai-responses" => openai_responses::stream(
            model,
            context,
            client,
            base_url,
            Some(api_key),
            &openai_responses::OpenAIResponsesOptions {
                base: options.clone(),
                ..Default::default()
            },
        ),
        "anthropic-messages" => anthropic_messages::stream(
            model,
            context,
            client,
            base_url,
            Some(api_key),
            &anthropic_messages::AnthropicOptions {
                base: options.clone(),
                ..Default::default()
            },
        ),
        "google-generative-ai" => google_generative_ai::stream(
            model,
            context,
            client,
            base_url,
            Some(api_key),
            &google_generative_ai::GoogleOptions::from_stream_options(options.clone()),
        ),
        "google-vertex" => {
            let mut model = model.clone();
            model.base_url = base_url.to_string();
            google_vertex::stream(
                &model,
                context,
                client,
                Some(api_key),
                &google_vertex::GoogleVertexOptions {
                    base: options.clone(),
                    ..Default::default()
                },
            )
        }
        "azure-openai-responses" => {
            let mut model = model.clone();
            model.base_url = base_url.to_string();
            azure_openai_responses::stream(
                &model,
                context,
                client,
                Some(api_key),
                &azure_openai_responses::AzureOpenAIResponsesOptions {
                    base: options.clone(),
                    azure_api_version: Some("v1".to_string()),
                    azure_deployment_name: Some("matrix-deployment".to_string()),
                    ..Default::default()
                },
            )
        }
        "openai-codex-responses" => {
            let mut model = model.clone();
            model.base_url = base_url.to_string();
            let mut options = options.clone();
            options.transport = Some("sse".to_string());
            openai_codex_responses::stream(
                &model,
                context,
                client,
                Some(api_key),
                &openai_codex_responses::OpenAICodexResponsesOptions {
                    base: options,
                    transport: Some("sse".to_string()),
                    ..Default::default()
                },
            )
        }
        "mistral-conversations" => {
            let mut model = model.clone();
            model.base_url = base_url.to_string();
            mistral_conversations::stream(
                &model,
                context,
                client,
                Some(api_key),
                &mistral_conversations::MistralOptions {
                    base: options.clone(),
                    ..Default::default()
                },
            )
        }
        "bedrock-converse-stream" => {
            let mut model = model.clone();
            model.base_url = base_url.to_string();
            bedrock_converse::stream(
                &model,
                context,
                client,
                Some(api_key),
                &bedrock_converse::BedrockOptions {
                    base: options.clone(),
                    region: Some("us-east-1".to_string()),
                    ..Default::default()
                },
            )
        }
        other => panic!("unsupported text API fixture {other}"),
    }
}

fn fixture_body(fixture: &ApiFixture) -> Vec<u8> {
    if let Some(body) = &fixture.success.body {
        return serde_json::to_vec(body).expect("fixture JSON body serializes");
    }
    if let Some(events) = &fixture.success.sse {
        let mut body = String::new();
        for event in events {
            if let Some(name) = &event.event {
                body.push_str("event: ");
                body.push_str(name);
                body.push('\n');
            }
            body.push_str("data: ");
            if let Some(raw) = event.data.as_str() {
                body.push_str(raw);
            } else {
                body.push_str(
                    &serde_json::to_string(&event.data).expect("fixture SSE data serializes"),
                );
            }
            body.push_str("\n\n");
        }
        return body.into_bytes();
    }
    let frames = fixture
        .success
        .frames
        .as_ref()
        .expect("fixture has a response body");
    frames
        .iter()
        .flat_map(|frame| encode_eventstream_frame(&frame.event_type, &frame.payload))
        .collect()
}

fn encode_eventstream_frame(event_type: &str, payload_json: &Value) -> Vec<u8> {
    let mut headers = Vec::new();
    let mut push_header = |name: &str, value: &str| {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(6);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    };
    push_header(":message-type", "event");
    push_header(":event-type", event_type);
    let payload = serde_json::to_vec(payload_json).expect("event-stream payload serializes");
    let total_length = 16 + headers.len() + payload.len() + 4;
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0x00, 0xC0, 0xDE, 0x00]);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

async fn spawn_mock_server(
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    response_headers: Vec<(String, String)>,
) -> (
    String,
    Arc<Mutex<Option<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind matrix fixture server");
    let address = listener
        .local_addr()
        .expect("matrix fixture server address");
    let captured = Arc::new(Mutex::new(None));
    let captured_server = Arc::clone(&captured);
    let content_type = content_type.to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("matrix fixture request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let count = socket
                .read(&mut buffer)
                .await
                .expect("read matrix fixture headers");
            assert!(count > 0, "fixture client closed before headers");
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers_text = String::from_utf8_lossy(&request[..header_end]).into_owned();
        let request_headers: BTreeMap<_, _> = headers_text
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        let content_length = headers_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let count = socket
                .read(&mut buffer)
                .await
                .expect("read matrix fixture body");
            assert!(count > 0, "fixture client closed before body");
            request.extend_from_slice(&buffer[..count]);
        }
        let path = headers_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        *captured_server.lock().expect("capture lock") = Some(CapturedRequest {
            path,
            headers: request_headers,
            body: request[header_end..header_end + content_length].to_vec(),
        });

        let reason = match status {
            200 => "OK",
            429 => "Too Many Requests",
            400 => "Bad Request",
            _ => "Fixture",
        };
        let mut response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        for (name, value) in response_headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write matrix fixture headers");
        socket
            .write_all(&body)
            .await
            .expect("write matrix fixture body");
    });
    (format!("http://{address}"), captured, server)
}

#[cfg(target_os = "linux")]
#[link(name = "zstd")]
unsafe extern "C" {
    fn ZSTD_getFrameContentSize(src: *const u8, src_size: usize) -> u64;
    fn ZSTD_decompress(
        dst: *mut u8,
        dst_capacity: usize,
        src: *const u8,
        compressed_size: usize,
    ) -> usize;
    fn ZSTD_isError(code: usize) -> u32;
}

fn request_json(request: &CapturedRequest) -> Vec<u8> {
    if request.headers.get("content-encoding").map(String::as_str) != Some("zstd") {
        return request.body.clone();
    }

    #[cfg(target_os = "linux")]
    {
        const CONTENT_SIZE_ERROR: u64 = u64::MAX;
        const CONTENT_SIZE_UNKNOWN: u64 = u64::MAX - 1;
        let size = unsafe { ZSTD_getFrameContentSize(request.body.as_ptr(), request.body.len()) };
        assert!(
            size != CONTENT_SIZE_ERROR && size != CONTENT_SIZE_UNKNOWN,
            "Codex zstd request must declare its content size"
        );
        let mut decoded = vec![0_u8; size as usize];
        let written = unsafe {
            ZSTD_decompress(
                decoded.as_mut_ptr(),
                decoded.len(),
                request.body.as_ptr(),
                request.body.len(),
            )
        };
        assert_eq!(
            unsafe { ZSTD_isError(written) },
            0,
            "Codex zstd decode failed"
        );
        decoded.truncate(written);
        decoded
    }
    #[cfg(not(target_os = "linux"))]
    {
        panic!("Codex zstd request decoding is unavailable on this target");
    }
}

async fn run_text_case(
    api: &str,
    model: &Model,
    api_key: &str,
    status: u16,
    body: Vec<u8>,
    content_type: &str,
) -> (CapturedRequest, AssistantMessage) {
    let mut model = model.clone();
    let response_headers = if api == "bedrock-converse-stream" {
        vec![("x-amzn-requestid".to_string(), "matrix-request".to_string())]
    } else {
        Vec::new()
    };
    let (base_url, captured, server) =
        spawn_mock_server(status, content_type, body, response_headers).await;
    model.base_url = base_url.clone();
    let options = stream_options(api_key);
    let stream = invoke_text_stream(
        api,
        &model,
        &context(),
        reqwest::Client::new(),
        &base_url,
        api_key,
        &options,
    );
    let (_, message) = stream.collect().await;
    server.await.expect("matrix fixture server task");
    let request = captured
        .lock()
        .expect("capture lock")
        .take()
        .expect("fixture captured request");
    (request, message)
}

fn assert_request(
    variant: &MatrixVariant,
    fixture: &ApiFixture,
    model: &Model,
    request: &CapturedRequest,
) {
    assert!(
        request.path.contains(&fixture.path_fragment),
        "{}/{} request path {:?} does not contain {:?}",
        variant.provider,
        variant.api,
        request.path,
        fixture.path_fragment
    );
    let body_bytes = request_json(request);
    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or_else(|error| {
        panic!(
            "{}/{} request body is not JSON: {error}; body={:?}",
            variant.provider, variant.api, body_bytes
        )
    });
    if let Some(field) = &fixture.model_field {
        assert_eq!(
            body.get(field).and_then(Value::as_str),
            Some(model.id.as_str())
        );
    }
    let required = match fixture.api.as_str() {
        "openai-completions" => ["model", "messages", "stream"].as_slice(),
        "openai-responses" | "azure-openai-responses" | "openai-codex-responses" => {
            ["model", "input", "stream"].as_slice()
        }
        "anthropic-messages" => ["model", "messages", "stream"].as_slice(),
        "google-generative-ai" | "google-vertex" => ["contents"].as_slice(),
        "mistral-conversations" => ["model", "messages", "stream"].as_slice(),
        "bedrock-converse-stream" => ["modelId", "messages"].as_slice(),
        "openrouter-images" => ["model", "messages", "stream", "modalities"].as_slice(),
        other => panic!("no request assertions for {other}"),
    };
    for field in required {
        assert!(
            body.get(*field).is_some(),
            "{}/{} request is missing {field}: {body}",
            variant.provider,
            variant.api
        );
    }
}

fn text_output(message: &AssistantMessage) -> String {
    message
        .content()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_usage(actual: &Usage, expected: &UsageFixture) {
    assert_eq!(actual.input, expected.input);
    assert_eq!(actual.output, expected.output);
    assert_eq!(actual.cache_read, expected.cache_read);
    assert_eq!(actual.cache_write, expected.cache_write);
    assert_eq!(actual.total_tokens, expected.total_tokens);
}

fn assert_success(
    variant: &MatrixVariant,
    fixture: &ApiFixture,
    model: &Model,
    message: &AssistantMessage,
) {
    assert_eq!(
        message.stop_reason(),
        Some(StopReason::Stop),
        "{}/{}",
        variant.provider,
        variant.api
    );
    assert_eq!(
        text_output(message),
        fixture.success.text,
        "{}/{}",
        variant.provider,
        variant.api
    );
    assert_eq!(
        message.response_id(),
        fixture.success.response_id.as_deref(),
        "{}/{}",
        variant.provider,
        variant.api
    );
    assert_usage(
        message
            .usage()
            .unwrap_or_else(|| panic!("{}/{} has no usage", variant.provider, variant.api)),
        &fixture.success.usage,
    );
    assert_eq!(message.model(), Some(model.id.as_str()));
}

fn assert_error(variant: &MatrixVariant, fixture: &ApiFixture, message: &AssistantMessage) {
    assert_eq!(
        message.stop_reason(),
        Some(StopReason::Error),
        "{}/{}",
        variant.provider,
        variant.api
    );
    assert!(
        message
            .error_message()
            .unwrap_or("")
            .contains(&fixture.error.contains),
        "{}/{} error {:?} does not contain {:?}",
        variant.provider,
        variant.api,
        message.error_message(),
        fixture.error.contains
    );
}

fn catalog_models_by_pair() -> BTreeMap<(String, String), Model> {
    builtin_providers()
        .into_iter()
        .flat_map(|provider| {
            provider
                .models
                .into_iter()
                .map(|model| ((model.provider.clone(), model.api.clone()), model))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn fixture_filename_for_api(api: &str) -> &'static str {
    match api {
        "openai-completions" => "openai-completions.json",
        "openai-responses" => "openai-responses.json",
        "anthropic-messages" => "anthropic-messages.json",
        "google-generative-ai" => "google-generative-ai.json",
        "google-vertex" => "google-vertex.json",
        "azure-openai-responses" => "azure-openai-responses.json",
        "openai-codex-responses" => "openai-codex-responses.json",
        "mistral-conversations" => "mistral-conversations.json",
        "bedrock-converse-stream" => "bedrock-converse-stream.json",
        "openrouter-images" => "openrouter-images.json",
        other => panic!("no fixture filename for {other}"),
    }
}

#[test]
fn fixture_index_covers_catalog_pairs_and_upstream_oracles() {
    let index = parse_index();
    assert_eq!(index.schema_version, 1);
    let catalog_pairs: BTreeSet<_> = catalog_models_by_pair().into_keys().collect();
    let indexed_pairs: BTreeSet<_> = index
        .variants
        .iter()
        .filter(|variant| variant.kind == "text")
        .map(|variant| (variant.provider.clone(), variant.api.clone()))
        .collect();
    assert_eq!(indexed_pairs, catalog_pairs);

    for variant in &index.variants {
        assert_eq!(
            variant.evidence_tier, "mock",
            "{} / {}",
            variant.provider, variant.api
        );
        assert!(!variant.upstream_oracle.is_empty());
        assert!(!variant.constructor_status.is_empty());
        let fixture = if variant.kind == "image" {
            load_api_fixture("openrouter-images.json")
        } else {
            load_api_fixture(&variant.fixture)
        };
        assert_eq!(fixture.api, variant.api);
        assert_eq!(variant.fixture, fixture_filename_for_api(&variant.api));
        for oracle in &variant.upstream_oracle {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(oracle);
            assert!(path.exists(), "missing upstream oracle {oracle}");
        }
    }

    let images = images::catalog_images("openrouter");
    assert!(!images.is_empty(), "OpenRouter image catalog is empty");
    assert!(index
        .variants
        .iter()
        .any(|variant| variant.kind == "image" && variant.api == "openrouter-images"));
}

#[tokio::test]
async fn provider_api_matrix_proves_request_stream_usage_and_error() {
    let index = parse_index();
    let models = catalog_models_by_pair();
    for variant in index
        .variants
        .iter()
        .filter(|variant| variant.kind == "text")
    {
        let fixture = load_api_fixture(&variant.fixture);
        let model = models
            .get(&(variant.provider.clone(), variant.api.clone()))
            .unwrap_or_else(|| {
                panic!(
                    "missing catalog model for {}/{}",
                    variant.provider, variant.api
                )
            });
        let api_key = if variant.api == "openai-codex-responses" {
            codex_token()
        } else {
            "matrix-key".to_string()
        };
        let (request, message) = run_text_case(
            &variant.api,
            model,
            &api_key,
            200,
            fixture_body(&fixture),
            &fixture.success.content_type,
        )
        .await;
        assert_request(variant, &fixture, model, &request);
        assert_success(variant, &fixture, model, &message);

        let error_body = serde_json::to_vec(&fixture.error.body).expect("error fixture serializes");
        let (request, message) = run_text_case(
            &variant.api,
            model,
            &api_key,
            fixture.error.status,
            error_body,
            &fixture.error.content_type,
        )
        .await;
        assert_request(variant, &fixture, model, &request);
        assert_error(variant, &fixture, &message);
    }
}

#[tokio::test]
async fn qwen_token_plan_registrations_round_trip_selected_catalog_models() {
    let fixture = load_api_fixture("openai-completions.json");
    let cases = [
        ("qwen-token-plan", "qwen3.7-max"),
        ("qwen-token-plan-cn", "qwen3.7-max"),
        ("qwen-token-plan-individual", "qwen3.8-max"),
    ];

    for (provider_id, model_id) in cases {
        let provider = builtin_providers()
            .into_iter()
            .find(|provider| provider.id == provider_id)
            .unwrap_or_else(|| panic!("missing provider {provider_id}"));
        let model = provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .unwrap_or_else(|| panic!("missing {provider_id}/{model_id}"));

        let (request, message) = run_text_case(
            "openai-completions",
            model,
            "qwen-test-key",
            200,
            fixture_body(&fixture),
            &fixture.success.content_type,
        )
        .await;
        let request_body: Value =
            serde_json::from_slice(&request_json(&request)).expect("Qwen request JSON");
        // `run_text_case` replaces the catalog base URL with a loopback
        // origin, so the OpenAI-compatible adaptor contributes only the
        // endpoint suffix here. The catalog's `/compatible-mode/v1` prefix
        // is separately asserted by the provider registration tests.
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer qwen-test-key")
        );
        assert_eq!(request_body["model"], model_id);
        assert_eq!(request_body["stream"], true);
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(message.stop_reason(), Some(StopReason::Stop));
        assert_eq!(
            message
                .content()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            fixture.success.text
        );
        let usage = message.usage().expect("Qwen response usage");
        assert_eq!(usage.input, fixture.success.usage.input);
        assert_eq!(usage.output, fixture.success.usage.output);
        assert_eq!(usage.total_tokens, fixture.success.usage.total_tokens);
        assert_eq!(usage.cost, Default::default());

        let (request, message) = run_text_case(
            "openai-completions",
            model,
            "qwen-test-key",
            fixture.error.status,
            serde_json::to_vec(&fixture.error.body).expect("error fixture serializes"),
            &fixture.error.content_type,
        )
        .await;
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert!(message
            .error_message()
            .unwrap_or("")
            .contains(&fixture.error.contains));
    }
}

#[tokio::test]
async fn qwen_token_plan_malformed_and_http_errors_are_terminal_and_redacted() {
    let cases = [
        ("qwen-token-plan", "qwen3.7-max"),
        ("qwen-token-plan-cn", "qwen3.7-max"),
        ("qwen-token-plan-individual", "qwen3.8-max"),
    ];
    for (provider_id, model_id) in cases {
        let provider = builtin_providers()
            .into_iter()
            .find(|provider| provider.id == provider_id)
            .unwrap_or_else(|| panic!("missing provider {provider_id}"));
        let model = provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .unwrap_or_else(|| panic!("missing {provider_id}/{model_id}"));
        let secret = "qwen-fixture-secret";

        let (request, malformed) = run_text_case(
            "openai-completions",
            model,
            secret,
            200,
            b"data: {malformed-qwen-fixture}\n\n".to_vec(),
            "text/event-stream",
        )
        .await;
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(malformed.stop_reason(), Some(StopReason::Error));
        assert!(malformed
            .error_message()
            .unwrap_or("")
            .contains("without finish_reason"));
        assert!(!malformed.error_message().unwrap_or("").contains(secret));

        let (request, http_error) = run_text_case(
            "openai-completions",
            model,
            secret,
            502,
            br#"{"error":{"message":"qwen fixture upstream failure"}}"#.to_vec(),
            "application/json",
        )
        .await;
        assert_eq!(request.path, "/chat/completions");
        assert_eq!(http_error.stop_reason(), Some(StopReason::Error));
        assert!(http_error
            .error_message()
            .unwrap_or("")
            .contains("qwen fixture upstream failure"));
        assert!(!http_error.error_message().unwrap_or("").contains(secret));
    }
}

#[tokio::test]
async fn qwen_token_plan_stream_simple_preserves_xhigh_wire_shape() {
    let fixture = load_api_fixture("openai-completions.json");
    for (provider_id, model_id) in [
        ("qwen-token-plan", "qwen3.8-max"),
        ("qwen-token-plan-cn", "qwen3.8-max"),
        ("qwen-token-plan-individual", "qwen3.8-max"),
    ] {
        let provider = builtin_providers()
            .into_iter()
            .find(|provider| provider.id == provider_id)
            .unwrap_or_else(|| panic!("missing provider {provider_id}"));
        let model = provider
            .models
            .iter()
            .find(|model| model.id == model_id)
            .unwrap_or_else(|| panic!("missing {provider_id}/{model_id}"))
            .clone();
        let (base_url, captured, server) = spawn_mock_server(
            200,
            &fixture.success.content_type,
            fixture_body(&fixture),
            Vec::new(),
        )
        .await;
        let options = SimpleStreamOptions {
            base: StreamOptions {
                base: ProviderRequestOptions {
                    api_key: Some("qwen-simple-key".to_string()),
                    max_retries: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            },
            reasoning: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        };
        let (_, message) = openai_completions::stream_simple(
            &model,
            &context(),
            reqwest::Client::new(),
            &base_url,
            Some("qwen-simple-key"),
            &options,
        )
        .collect()
        .await;
        server
            .await
            .expect("Qwen stream_simple fixture server task");
        let request = captured
            .lock()
            .expect("capture lock")
            .take()
            .expect("Qwen stream_simple fixture captured request");
        let body: Value = serde_json::from_slice(&request.body).expect("Qwen request JSON");
        assert_eq!(request.path, "/chat/completions", "{provider_id}");
        assert_eq!(body["enable_thinking"], true, "{provider_id}");
        assert_eq!(body["reasoning_effort"], "xhigh", "{provider_id}");
        assert!(body.get("thinking").is_none(), "{provider_id}");
        assert_eq!(
            message.stop_reason(),
            Some(StopReason::Stop),
            "{provider_id}: {:?}",
            message.error_message()
        );
    }
}

#[tokio::test]
async fn openrouter_images_fixture_proves_request_response_usage_and_error() {
    let fixture = load_api_fixture("openrouter-images.json");
    let catalog_model = images::catalog_images("openrouter")
        .into_iter()
        .next()
        .expect("OpenRouter image model");
    let context = ImagesContext {
        input: vec![ContentBlock::text("draw a matrix")],
    };
    let options = ImagesOptions {
        api_key: Some("matrix-key".to_string()),
        max_retries: Some(0),
        ..Default::default()
    };
    let success_body = serde_json::to_vec(fixture.success.body.as_ref().expect("image body"))
        .expect("image success fixture serializes");
    let (base_url, captured, server) =
        spawn_mock_server(200, &fixture.success.content_type, success_body, Vec::new()).await;
    let mut model = catalog_model.clone();
    model.base_url = base_url;
    let result =
        openrouter_images::generate_images(&model, &context, &options, reqwest::Client::new())
            .await;
    server.await.expect("image fixture server task");
    let request = captured
        .lock()
        .expect("capture lock")
        .take()
        .expect("image fixture captured request");
    let request_body: Value = serde_json::from_slice(&request.body).expect("image request JSON");
    assert!(request.path.contains(&fixture.path_fragment));
    assert_eq!(request_body["model"], model.id);
    assert_eq!(request_body["stream"], false);
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(
        result.response_id.as_deref(),
        fixture.success.response_id.as_deref()
    );
    assert_usage(
        result.usage.as_ref().expect("image usage"),
        &fixture.success.usage,
    );
    assert!(result.output.iter().any(
        |block| matches!(block, ContentBlock::Text { text, .. } if text == &fixture.success.text)
    ));
    assert!(result
        .output
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { data, .. } if data == "AA==")));

    let error_body =
        serde_json::to_vec(&fixture.error.body).expect("image error fixture serializes");
    let (base_url, captured, server) = spawn_mock_server(
        fixture.error.status,
        &fixture.error.content_type,
        error_body,
        Vec::new(),
    )
    .await;
    model.base_url = base_url;
    let result =
        openrouter_images::generate_images(&model, &context, &options, reqwest::Client::new())
            .await;
    server.await.expect("image error fixture server task");
    let request = captured
        .lock()
        .expect("capture lock")
        .take()
        .expect("image error fixture captured request");
    assert!(request.path.contains(&fixture.path_fragment));
    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    assert!(result
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains(&fixture.error.contains));
}

#[tokio::test]
async fn no_api_controls_reject_unsupported_by_api_variants() {
    let fixture = parse_no_api_fixture();
    assert_eq!(fixture.evidence_tier, "unit");
    let providers = builtin_providers();
    let context = Context::default();
    for control in fixture.controls {
        let provider = providers
            .iter()
            .find(|provider| provider.id == control.provider)
            .unwrap_or_else(|| panic!("missing provider {}", control.provider));
        let mut model = provider
            .models
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("provider {} has no model", control.provider));
        model.api = control.api.clone();
        let stream = provider.stream(&model, &context, None);
        let (_, message) = stream.collect().await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert_eq!(message.error_message(), Some(control.expected.as_str()));

        let simple = SimpleStreamOptions::default();
        let (_, message) = provider
            .stream_simple(&model, &context, Some(&simple))
            .collect()
            .await;
        assert_eq!(message.stop_reason(), Some(StopReason::Error));
        assert_eq!(message.error_message(), Some(control.expected.as_str()));
    }
}
