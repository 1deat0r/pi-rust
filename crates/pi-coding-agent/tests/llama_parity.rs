#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

#[allow(dead_code)]
#[path = "../src/core/llama.rs"]
mod llama;

// `llama.rs` is also compiled as a standalone integration-test module. Give
// that fixture the same path resolver used by the production crate so its
// Hugging Face credential helper remains buildable under all-target checks.
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use llama::{
    find_huggingface_token_from, format_bytes, llama_credential, normalize_llama_server_url,
    HuggingFaceClient, HuggingFaceGated, LlamaApiKeyAuth, LlamaCancellation, LlamaClient,
    LlamaError, LlamaManagerAction, LlamaModelAction, LlamaModelInfo, LlamaModelStatus,
    LlamaModelStatusValue, LlamaProviderController, LlamaWaitOptions,
};
use pi_ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthEvent, AuthInteraction, AuthPrompt, CredentialStore,
    InMemoryCredentialStore,
};
use pi_ai::models::{
    create_models, CreateModelsOptions, InMemoryModelsStore, ModelsRefreshOptions, ModelsStore,
};
use pi_ai::types::{AssistantMessageEvent, Context, StreamOptions};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug)]
enum ServerMode {
    Router,
    InvalidRouter,
    InvalidCatalog,
    Error,
    StringError,
    SseError,
    Delayed,
    LoadFailed,
    LoadExited,
    HuggingFace,
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Load,
    Download,
}

#[derive(Debug, Clone)]
struct RequestRecord {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

struct ServerState {
    mode: ServerMode,
    operation: Option<Operation>,
    status: LlamaModelStatusValue,
    requests: Vec<RequestRecord>,
}

struct TestServer {
    base_url: String,
    state: Arc<Mutex<ServerState>>,
    task: tokio::task::JoinHandle<()>,
}

struct PromptInteraction {
    answers: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<AuthPrompt>>,
}

impl PromptInteraction {
    fn new(answers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().map(Into::into).collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }
}

impl AuthInteraction for PromptInteraction {
    fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String> {
        self.prompts.lock().unwrap().push(prompt.clone());
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "test prompt exhausted".to_owned())
    }

    fn notify(&self, _event: &AuthEvent) {}
}

impl TestServer {
    async fn new(mode: ServerMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(ServerState {
            mode,
            operation: None,
            status: LlamaModelStatusValue::Unloaded,
            requests: Vec::new(),
        }));
        let state_for_task = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = state_for_task.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
        });
        Self {
            base_url: format!("http://{address}"),
            state,
            task,
        }
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<RequestRecord>> {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 2048];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|value| value == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(None);
        }
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0_usize;
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    while buffer.len() < header_end + content_length {
        let mut chunk = [0_u8; 2048];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_end = (header_end + content_length).min(buffer.len());
    Ok(Some(RequestRecord {
        method,
        path,
        authorization,
        body: String::from_utf8_lossy(&buffer[header_end..body_end]).to_string(),
    }))
}

async fn send_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await
}

async fn send_json(stream: &mut TcpStream, status: &str, value: Value) -> std::io::Result<()> {
    send_response(
        stream,
        status,
        "application/json",
        &serde_json::to_vec(&value).unwrap(),
    )
    .await
}

fn catalog_value(status: LlamaModelStatusValue) -> Value {
    let status = serde_json::to_value(status).unwrap();
    json!({
        "data": [
            {
                "id": "local.gguf",
                "aliases": ["local"],
                "status": {"value": status},
                "architecture": {"input_modalities": ["text", "image"]},
                "meta": {"n_ctx": 65536, "n_ctx_train": 32768}
            },
            {
                "id": "sleep.gguf",
                "status": {"value": "sleeping"},
                "meta": {"n_ctx_train": 8192}
            },
            {
                "id": "cold.gguf",
                "status": {"value": "unloaded"}
            },
            {
                "id": "download.gguf",
                "status": {"value": status}
            }
        ]
    })
}

async fn wait_for_operation(state: &Arc<Mutex<ServerState>>) -> Option<Operation> {
    for _ in 0..40 {
        if let Some(operation) = state.lock().unwrap().operation {
            return Some(operation);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

async fn handle_connection(
    mut stream: TcpStream,
    state: Arc<Mutex<ServerState>>,
) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let mode = {
        let mut state_guard = state.lock().unwrap();
        let mode = state_guard.mode;
        state_guard.requests.push(request.clone());
        mode
    };

    if request.path.starts_with("/v1/chat/completions") {
        let body = concat!(
            "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).await?;
        return stream.write_all(body.as_bytes()).await;
    }

    if matches!(mode, ServerMode::HuggingFace) {
        if request.path.starts_with("/api/models?") {
            return send_json(
                &mut stream,
                "200 OK",
                json!([{"id":"org/llama-3","downloads":1234}]),
            )
            .await;
        }
        if request.path.starts_with("/api/models/org/repo") {
            return send_json(
                &mut stream,
                "200 OK",
                json!({
                    "id": "org/repo",
                    "gated": "auto",
                    "siblings": [
                        {"rfilename":"org-repo.Q4_K_M-00001-of-00002.gguf","size":2000},
                        {"rfilename":"org-repo.Q4_K_M-00002-of-00002.gguf","size":3000},
                        {"rfilename":"org-repo.Q5_K_M.gguf","size":6000},
                        {"rfilename":"org-repo-mmproj-f16.gguf","size":99}
                    ]
                }),
            )
            .await;
        }
        return send_json(&mut stream, "404 Not Found", json!({"error":"not found"})).await;
    }

    match mode {
        ServerMode::InvalidRouter => {
            send_json(
                &mut stream,
                "200 OK",
                json!({"data":[{"id":"bad","status":{"value":"unknown"}}]}),
            )
            .await
        }
        ServerMode::InvalidCatalog => {
            send_json(&mut stream, "200 OK", json!({"data":{"not":"an array"}})).await
        }
        ServerMode::Error => {
            send_json(
                &mut stream,
                "503 Service Unavailable",
                json!({"error":{"message":"router offline"}}),
            )
            .await
        }
        ServerMode::StringError => {
            send_json(
                &mut stream,
                "503 Service Unavailable",
                json!({"error":"router offline"}),
            )
            .await
        }
        ServerMode::SseError => {
            send_json(
                &mut stream,
                "502 Bad Gateway",
                json!({"error":{"message":"event stream unavailable"}}),
            )
            .await
        }
        ServerMode::Delayed => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            send_json(
                &mut stream,
                "200 OK",
                catalog_value(LlamaModelStatusValue::Loaded),
            )
            .await
        }
        ServerMode::LoadFailed | ServerMode::LoadExited => {
            if request.path == "/models/sse" {
                return send_response(&mut stream, "200 OK", "text/event-stream", &[]).await;
            }
            if request.method == "POST" && request.path == "/models/load" {
                state.lock().unwrap().status = LlamaModelStatusValue::Unloaded;
                return send_json(&mut stream, "200 OK", json!({"ok":true})).await;
            }
            if request.method == "GET" && request.path.starts_with("/models") {
                let mut payload = catalog_value(LlamaModelStatusValue::Unloaded);
                let status = payload["data"][0]["status"].as_object_mut().unwrap();
                if matches!(mode, ServerMode::LoadFailed) {
                    status.insert("failed".to_owned(), json!(true));
                } else {
                    status.insert("exit_code".to_owned(), json!(17));
                }
                return send_json(&mut stream, "200 OK", payload).await;
            }
            send_json(&mut stream, "404 Not Found", json!({"error":"not found"})).await
        }
        ServerMode::Router => {
            if request.path == "/models/sse" {
                let operation = wait_for_operation(&state).await;
                let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                stream.write_all(header.as_bytes()).await?;
                match operation {
                    Some(Operation::Load) => {
                        let progress = json!({
                            "model":"local.gguf",
                            "event":"status_change",
                            "data":{"status":"loading","progress":{"stages":["load_text"],"current":"load_text","value":0.5}}
                        });
                        stream
                            .write_all(format!("data: {progress}\n\n").as_bytes())
                            .await?;
                        {
                            let mut state_guard = state.lock().unwrap();
                            state_guard.status = LlamaModelStatusValue::Loaded;
                        }
                        let loaded = json!({"model":"local.gguf","event":"status_change","data":{"status":"loaded"}});
                        stream
                            .write_all(format!("data: {loaded}\n\n").as_bytes())
                            .await?;
                    }
                    Some(Operation::Download) => {
                        // Publish the terminal catalog state before the SSE
                        // frames to exercise the real polling/event ordering
                        // race: completion must not drop earlier progress.
                        state.lock().unwrap().status = LlamaModelStatusValue::Unloaded;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let progress = json!({
                            "model":"download.gguf",
                            "event":"download_progress",
                            "data":{"progress":{"weights":{"done":5120,"total":10240}}}
                        });
                        stream
                            .write_all(format!("data: {progress}\n\n").as_bytes())
                            .await?;
                        let finished = json!({"model":"download.gguf","event":"download_finished"});
                        stream
                            .write_all(format!("data: {finished}\n\n").as_bytes())
                            .await?;
                    }
                    None => {}
                }
                state.lock().unwrap().operation = None;
                return Ok(());
            }
            if request.method == "GET" && request.path.starts_with("/models") {
                let status = state.lock().unwrap().status;
                return send_json(&mut stream, "200 OK", catalog_value(status)).await;
            }
            if request.method == "POST" && request.path == "/models/load" {
                {
                    let mut state_guard = state.lock().unwrap();
                    state_guard.operation = Some(Operation::Load);
                    state_guard.status = LlamaModelStatusValue::Loading;
                }
                return send_json(&mut stream, "200 OK", json!({"ok":true})).await;
            }
            if request.method == "POST" && request.path == "/models" {
                {
                    let mut state_guard = state.lock().unwrap();
                    state_guard.operation = Some(Operation::Download);
                    state_guard.status = LlamaModelStatusValue::Downloading;
                }
                return send_json(&mut stream, "200 OK", json!({"ok":true})).await;
            }
            if request.method == "POST" && request.path == "/models/unload" {
                state.lock().unwrap().status = LlamaModelStatusValue::Unloaded;
                return send_json(&mut stream, "200 OK", json!({"ok":true})).await;
            }
            send_json(&mut stream, "404 Not Found", json!({"error":"not found"})).await
        }
        ServerMode::HuggingFace => unreachable!(),
    }
}

fn model_info(id: &str, status: LlamaModelStatusValue) -> LlamaModelInfo {
    LlamaModelInfo {
        id: id.to_owned(),
        aliases: Vec::new(),
        status: LlamaModelStatus {
            value: status,
            args: None,
            failed: None,
            exit_code: None,
            progress: None,
        },
        architecture: None,
        source: None,
        meta: Some(llama::LlamaModelMeta {
            n_ctx: None,
            n_ctx_train: Some(65_536),
            size: None,
            ftype: None,
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_router_catalog_normalizes_url_and_sends_auth() {
    let server = TestServer::new(ServerMode::Router).await;
    let client = LlamaClient::new(&server.base_url, Some("secret")).unwrap();
    assert_eq!(client.server_url(), server.base_url);
    let models = client
        .list(llama::LlamaListOptions {
            reload: true,
            signal: None,
        })
        .await
        .unwrap();
    assert_eq!(models.len(), 4);
    assert_eq!(models[0].status.value, LlamaModelStatusValue::Unloaded);
    let request = server
        .requests()
        .into_iter()
        .find(|request| request.path.starts_with("/models"))
        .unwrap();
    assert_eq!(request.path, "/models?reload=1");
    assert_eq!(request.authorization.as_deref(), Some("Bearer secret"));
    assert!(request.body.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn router_errors_cancellation_and_timeout_are_typed() {
    let error_server = TestServer::new(ServerMode::Error).await;
    let error = LlamaClient::new(&error_server.base_url, None)
        .unwrap()
        .list(Default::default())
        .await
        .unwrap_err();
    assert!(matches!(&error, LlamaError::Http { status: 503, .. }));
    assert_eq!(error.to_string(), "router offline");

    let string_error_server = TestServer::new(ServerMode::StringError).await;
    let error = LlamaClient::new(&string_error_server.base_url, None)
        .unwrap()
        .list(Default::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "llama.cpp returned HTTP 503");

    let invalid_catalog_server = TestServer::new(ServerMode::InvalidCatalog).await;
    let error = LlamaClient::new(&invalid_catalog_server.base_url, None)
        .unwrap()
        .list(Default::default())
        .await
        .unwrap_err();
    assert!(matches!(&error, LlamaError::InvalidCatalog(_)));
    assert_eq!(
        error.to_string(),
        "llama.cpp returned an invalid model catalog"
    );

    let sse_error_server = TestServer::new(ServerMode::SseError).await;
    let error = LlamaClient::new(&sse_error_server.base_url, None)
        .unwrap()
        .watch(None, |_| {})
        .await
        .unwrap_err();
    assert!(matches!(&error, LlamaError::SseHttp { status: 502 }));
    assert_eq!(error.to_string(), "llama.cpp SSE returned HTTP 502");

    let invalid_server = TestServer::new(ServerMode::InvalidRouter).await;
    let error = LlamaClient::new(&invalid_server.base_url, None)
        .unwrap()
        .list(Default::default())
        .await
        .unwrap_err();
    assert!(matches!(error, LlamaError::NotRouterMode));

    let delayed_server = TestServer::new(ServerMode::Delayed).await;
    let cancellation: LlamaCancellation = Arc::new(AtomicBool::new(false));
    let cancel_client = LlamaClient::new(&delayed_server.base_url, None).unwrap();
    let cancel_signal = cancellation.clone();
    let cancellation_task = tokio::spawn(async move {
        cancel_client
            .list(llama::LlamaListOptions {
                reload: false,
                signal: Some(cancel_signal),
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancellation.store(true, Ordering::SeqCst);
    assert!(matches!(
        cancellation_task.await.unwrap(),
        Err(LlamaError::Cancelled)
    ));

    let timeout_client =
        LlamaClient::with_timeout(&delayed_server.base_url, None, Duration::from_millis(20))
            .unwrap();
    assert!(matches!(
        timeout_client.list(Default::default()).await,
        Err(LlamaError::Timeout)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_key_login_prompts_url_and_secret_validates_and_persists_env() {
    let server = TestServer::new(ServerMode::Router).await;
    assert_eq!(LlamaApiKeyAuth.name(), "llama.cpp server");
    let interaction = PromptInteraction::new([
        format!("{}/v1/", server.base_url),
        " login-secret ".to_owned(),
    ]);
    let credential = LlamaApiKeyAuth.login(&interaction).unwrap();
    assert_eq!(credential.key.as_deref(), Some("login-secret"));
    assert_eq!(
        credential
            .env
            .as_ref()
            .and_then(|environment| environment.get("LLAMA_BASE_URL"))
            .map(String::as_str),
        Some(server.base_url.as_str())
    );
    let prompts = interaction.prompts.lock().unwrap();
    assert!(matches!(prompts.first(), Some(AuthPrompt::Text { .. })));
    assert!(matches!(prompts.get(1), Some(AuthPrompt::Secret { .. })));
    let request = server
        .requests()
        .into_iter()
        .find(|request| request.path == "/models")
        .unwrap();
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer login-secret")
    );
}

#[test]
fn stored_blank_url_falls_back_to_ambient_url_without_leaking_key() {
    let mut stored_env = BTreeMap::new();
    stored_env.insert("LLAMA_BASE_URL".to_owned(), "  ".to_owned());
    let credential = ApiKeyCredential {
        key: None,
        env: Some(stored_env),
    };
    let context = pi_ai::auth::AuthContext {
        env: Arc::new(|name| match name {
            "LLAMA_BASE_URL" => Some("http://ambient.example:8080".to_owned()),
            "LLAMA_API_KEY" => Some("ambient-secret".to_owned()),
            _ => None,
        }),
        file_exists: Arc::new(|_| false),
    };

    let resolved = LlamaApiKeyAuth
        .resolve(&context, Some(&credential))
        .unwrap();
    assert_eq!(
        resolved.auth.base_url.as_deref(),
        Some("http://ambient.example:8080/v1")
    );
    assert_eq!(resolved.auth.api_key.as_deref(), Some("ambient-secret"));
    assert_eq!(resolved.source.as_deref(), Some("stored credential"));
    assert_eq!(
        resolved
            .env
            .as_ref()
            .and_then(|environment| environment.get("LLAMA_BASE_URL"))
            .map(String::as_str),
        Some("http://ambient.example:8080")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_download_and_sse_progress_use_real_loopback_http() {
    let server = TestServer::new(ServerMode::Router).await;
    let client = LlamaClient::new(&server.base_url, Some("secret")).unwrap();
    client.download("download.gguf", None).await.unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_callback = events.clone();
    client
        .watch(None, move |event| {
            events_for_callback.lock().unwrap().push(event.event);
        })
        .await
        .unwrap();
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event == "download_finished"));
    let progress = Arc::new(Mutex::new(Vec::new()));
    let progress_for_callback = progress.clone();
    let loaded = client
        .load_and_wait(
            "local.gguf",
            LlamaWaitOptions {
                timeout: Duration::from_secs(2),
                poll_interval: Duration::from_millis(10),
                signal: None,
            },
            move |value| progress_for_callback.lock().unwrap().push(value),
        )
        .await
        .unwrap();
    assert_eq!(loaded.status.value, LlamaModelStatusValue::Loaded);
    assert!(progress
        .lock()
        .unwrap()
        .iter()
        .any(|value| value.message == "Loading load text" && value.ratio == Some(0.5)));
    client
        .unload_and_wait(
            "local.gguf",
            LlamaWaitOptions {
                timeout: Duration::from_secs(2),
                poll_interval: Duration::from_millis(10),
                signal: None,
            },
        )
        .await
        .unwrap();

    let download_progress = Arc::new(Mutex::new(Vec::new()));
    let download_progress_for_callback = download_progress.clone();
    let mixed_download_progress = llama::parse_download_progress(Some(&json!({
        "progress": {
            "weights": {"done": 5120, "total": 10240},
            "malformed": {"done": "not-a-number", "total": 99},
            "partial": {"done": 1}
        }
    })))
    .unwrap();
    assert_eq!(mixed_download_progress.ratio, Some(0.5));
    assert_eq!(
        mixed_download_progress.detail.as_deref(),
        Some("5.00 KiB / 10.0 KiB")
    );
    assert!(llama::parse_download_progress(Some(&json!({
        "progress": {"malformed": {"done": "not-a-number", "total": 99}}
    })))
    .is_none());
    let unwrapped_download_progress = llama::parse_download_progress(Some(&json!({
        "weights": {"done": 5120, "total": 10240}
    })))
    .unwrap();
    assert_eq!(unwrapped_download_progress.ratio, Some(0.5));
    assert!(llama::parse_download_progress(Some(&json!({
        "progress": {"empty": {"done": 0, "total": 0}}
    })))
    .is_none());
    let downloaded = client
        .download_and_wait(
            "download.gguf",
            LlamaWaitOptions {
                timeout: Duration::from_secs(2),
                poll_interval: Duration::from_millis(10),
                signal: None,
            },
            move |value| download_progress_for_callback.lock().unwrap().push(value),
        )
        .await
        .unwrap();
    assert_eq!(downloaded.len(), 4);
    assert!(download_progress
        .lock()
        .unwrap()
        .iter()
        .any(|value| value.ratio == Some(0.5)
            && value.detail.as_deref() == Some("5.00 KiB / 10.0 KiB")));
    assert!(server
        .requests()
        .iter()
        .any(|request| request.method == "POST" && request.path == "/models/load"));
    assert!(server
        .requests()
        .iter()
        .any(|request| request.method == "POST" && request.path == "/models"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_wait_preserves_upstream_failure_diagnostics() {
    for (mode, expected) in [
        (ServerMode::LoadFailed, "Model failed to load"),
        (ServerMode::LoadExited, "Model exited with code 17"),
    ] {
        let server = TestServer::new(mode).await;
        let error = LlamaClient::new(&server.base_url, None)
            .unwrap()
            .load_and_wait(
                "local.gguf",
                LlamaWaitOptions {
                    timeout: Duration::from_secs(1),
                    poll_interval: Duration::from_millis(10),
                    signal: None,
                },
                |_| {},
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn huggingface_search_details_and_token_lookup_are_real() {
    let server = TestServer::new(ServerMode::HuggingFace).await;
    let client = HuggingFaceClient::with_base_url(Some("hf_secret"), &server.base_url).unwrap();
    let results = client.search("llama 3", None).await.unwrap();
    assert_eq!(results[0].id, "org/llama-3");
    let details = client.details("org/repo", None).await.unwrap();
    assert_eq!(details.gated, HuggingFaceGated::Auto);
    assert_eq!(details.quantizations[0].name, "Q4_K_M");
    assert_eq!(details.quantizations[0].size, Some(5000));
    assert_eq!(details.quantizations[1].name, "Q5_K_M");
    assert_eq!(details.quantizations[1].size, Some(6000));
    let requests = server.requests();
    assert!(requests.iter().any(|request| {
        request.path.contains("search=llama%203")
            && request.path.contains("filter=gguf")
            && request.authorization.as_deref() == Some("Bearer hf_secret")
    }));
    assert!(requests
        .iter()
        .any(|request| request.path == "/api/models/org/repo?blobs=true"));

    let mut environment = BTreeMap::new();
    environment.insert("HF_TOKEN".to_owned(), "  direct-token  ".to_owned());
    assert_eq!(
        find_huggingface_token_from(&environment, None).as_deref(),
        Some("direct-token")
    );
    assert_eq!(format_bytes(5120), "5.00 KiB");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_controller_registers_dynamic_models_and_openai_inference() {
    let server = TestServer::new(ServerMode::Router).await;
    let controller = LlamaProviderController::new();
    let provider = controller.provider();
    assert_eq!(provider.id, "llama.cpp");
    assert_eq!(provider.name, "llama.cpp");
    let catalog = vec![
        model_info("loaded.gguf", LlamaModelStatusValue::Loaded),
        model_info("cold.gguf", LlamaModelStatusValue::Unloaded),
    ];
    let models = controller.set_catalog(catalog, &server.base_url).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "loaded.gguf");
    assert_eq!(models[0].name, "loaded.gguf");
    assert_eq!(models[0].provider, "llama.cpp");
    assert_eq!(models[0].base_url, format!("{}/v1", server.base_url));
    assert_eq!(models[0].context_window, 65_536);
    assert_eq!(models[0].max_tokens, 65_536);
    assert!(models[0].input.contains(&pi_ai::model::ModelInput::Text));
    let options = controller.selection_options(&controller.catalog());
    assert!(options.iter().any(|option| {
        option.action
            == LlamaManagerAction::Model {
                id: "loaded.gguf".to_owned(),
                action: LlamaModelAction::Unload,
            }
    }));

    let models_facade = create_models(CreateModelsOptions::default());
    controller.register_into(&models_facade);
    assert!(models_facade.get_provider("llama.cpp").is_some());
    let mut stream_options = StreamOptions::default();
    stream_options.base.api_key = Some("secret".to_owned());
    let stream = models_facade.stream(&models[0], &Context::default(), Some(&stream_options));
    let mut text = String::new();
    let final_message = stream
        .for_each(|event| {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                text.push_str(&delta);
            }
        })
        .await;
    assert_eq!(text, "hello");
    assert_eq!(
        final_message.stop_reason(),
        Some(pi_ai::types::StopReason::Stop)
    );
    let inference_request = server
        .requests()
        .into_iter()
        .find(|request| request.path == "/v1/chat/completions")
        .unwrap();
    assert_eq!(
        inference_request.authorization.as_deref(),
        Some("Bearer secret")
    );
    assert!(inference_request.body.contains("loaded.gguf"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_refresh_publishes_live_catalog_and_restores_cache_offline() {
    let server = TestServer::new(ServerMode::Router).await;
    let credentials = Arc::new(InMemoryCredentialStore::new());
    let models_store = Arc::new(InMemoryModelsStore::new());
    let credential = llama_credential(&server.base_url, Some("secret".to_owned())).unwrap();
    credentials.modify("llama.cpp", &|_| Some(credential.clone()));

    let controller = LlamaProviderController::new();
    let models = create_models(CreateModelsOptions {
        credentials: Some(credentials.clone()),
        models_store: Some(models_store.clone()),
        auth_context: None,
    });
    controller.register_into(&models);
    let refresh = models
        .refresh(ModelsRefreshOptions {
            allow_network: true,
            providers: Some(vec!["llama.cpp".to_owned()]),
            force: true,
            signal: None,
        })
        .await;
    assert!(refresh.errors.is_empty());
    assert_eq!(models.get_models(Some("llama.cpp")).len(), 1);
    assert_eq!(models_store.read("llama.cpp").unwrap().models.len(), 1);
    assert!(server
        .requests()
        .iter()
        .any(|request| request.path == "/models?reload=1"));

    let cached_controller = LlamaProviderController::new();
    let cached_models = create_models(CreateModelsOptions {
        credentials: Some(credentials),
        models_store: Some(models_store),
        auth_context: None,
    });
    cached_controller.register_into(&cached_models);
    let offline = cached_models
        .refresh(ModelsRefreshOptions {
            allow_network: false,
            providers: Some(vec!["llama.cpp".to_owned()]),
            force: false,
            signal: None,
        })
        .await;
    assert!(offline.errors.is_empty());
    assert_eq!(cached_models.get_models(Some("llama.cpp")).len(), 1);
}

#[test]
fn url_normalization_rejects_non_http_and_strips_v1() {
    assert_eq!(
        normalize_llama_server_url("http://127.0.0.1:8080/prefix/v1/").unwrap(),
        "http://127.0.0.1:8080/prefix"
    );
    assert!(matches!(
        normalize_llama_server_url("ftp://127.0.0.1:8080"),
        Err(LlamaError::InvalidScheme)
    ));
}
