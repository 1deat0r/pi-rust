//! Native llama.cpp router and Hugging Face support.
//!
//! This module deliberately stops at a renderer-neutral boundary.  It owns the
//! real HTTP protocol, model catalog, provider construction, and cancellation
//! semantics; a host application can decide how to expose the typed selection
//! actions to an interactive UI.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use reqwest::{header, Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use pi_ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthInteraction, AuthPrompt, AuthResult,
    Credential, ModelAuth, ProviderAuth,
};
use pi_ai::error::PiAiError;
use pi_ai::model::{Model, ModelInput};
use pi_ai::models::{
    create_provider, CreateProviderOptions, Models, ModelsPersistence, ModelsPublication,
    ModelsStoreEntry, Provider, ProviderApiSpec, ProviderStreams, RefreshModelsContext,
};
use pi_ai::types::{ProviderEnv, SimpleStreamOptions, StreamOptions};

pub const LLAMA_PROVIDER_ID: &str = "llama.cpp";
pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080";
pub const DEFAULT_HUGGINGFACE_URL: &str = "https://huggingface.co";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Error)]
pub enum LlamaError {
    #[error("Server URL must use http or https")]
    InvalidScheme,
    #[error("invalid server URL: {0}")]
    InvalidServerUrl(String),
    #[error("llama.cpp request was cancelled")]
    Cancelled,
    #[error("llama.cpp request timed out")]
    Timeout,
    #[error("llama.cpp transport failed: {0}")]
    Transport(String),
    #[error("{message}")]
    Http { status: u16, message: String },
    #[error("llama.cpp SSE returned HTTP {status}")]
    SseHttp { status: u16 },
    #[error("Server is not running in llama.cpp router mode")]
    NotRouterMode,
    #[error("llama.cpp returned an invalid model catalog")]
    InvalidCatalog(String),
    #[error("invalid llama.cpp request: {0}")]
    InvalidRequest(String),
    #[error("{0}")]
    OperationFailed(String),
    #[error("Hugging Face returned HTTP {status}: {message}")]
    HuggingFaceHttp { status: u16, message: String },
    #[error("Hugging Face returned an invalid response: {0}")]
    InvalidHuggingFaceResponse(String),
}

pub type LlamaCancellation = Arc<AtomicBool>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlamaModelStatusValue {
    Unloaded,
    Loading,
    Loaded,
    Downloading,
    Sleeping,
}

impl LlamaModelStatusValue {
    pub fn is_selectable(self) -> bool {
        matches!(self, Self::Loaded | Self::Sleeping)
    }

    pub fn is_loaded(self) -> bool {
        matches!(self, Self::Loaded | Self::Sleeping)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlamaProgressBytes {
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlamaModelStatus {
    pub value: LlamaModelStatusValue,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub failed: Option<bool>,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub progress: Option<BTreeMap<String, LlamaProgressBytes>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlamaArchitecture {
    #[serde(default)]
    pub input_modalities: Option<Vec<String>>,
    #[serde(default)]
    pub output_modalities: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlamaModelMeta {
    #[serde(default)]
    pub n_ctx: Option<u64>,
    #[serde(default)]
    pub n_ctx_train: Option<u64>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub ftype: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlamaModelInfo {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub status: LlamaModelStatus,
    #[serde(default)]
    pub architecture: Option<LlamaArchitecture>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub meta: Option<LlamaModelMeta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlamaModelEvent {
    pub model: String,
    pub event: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlamaProgress {
    pub message: String,
    pub ratio: Option<f64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlamaModelAction {
    Load,
    Unload,
    Observe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlamaManagerAction {
    Model {
        id: String,
        action: LlamaModelAction,
    },
    Download,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaSelectionOption {
    pub label: String,
    pub description: String,
    pub action: LlamaManagerAction,
}

#[derive(Debug, Clone, Default)]
pub struct LlamaListOptions {
    pub reload: bool,
    pub signal: Option<LlamaCancellation>,
}

#[derive(Debug, Clone)]
pub struct LlamaWaitOptions {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub signal: Option<LlamaCancellation>,
}

impl Default for LlamaWaitOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_WAIT_TIMEOUT,
            poll_interval: Duration::from_millis(250),
            signal: None,
        }
    }
}

#[derive(Clone)]
struct HttpClient {
    client: reqwest::Client,
    bearer: Option<String>,
}

struct HttpResponse {
    status: StatusCode,
    headers: header::HeaderMap,
    body: Vec<u8>,
}

impl HttpClient {
    fn new(timeout: Duration, bearer: Option<&str>) -> Result<Self, LlamaError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| LlamaError::Transport(error.to_string()))?;
        let bearer = bearer
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        Ok(Self { client, bearer })
    }

    fn request_builder(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> reqwest::RequestBuilder {
        let mut request = self.client.request(method, url);
        if let Some(bearer) = &self.bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .json(body);
        }
        request
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
        signal: Option<LlamaCancellation>,
        deadline: Option<Instant>,
    ) -> Result<HttpResponse, LlamaError> {
        let response = await_with_controls(
            self.request_builder(method, url, body.as_ref()).send(),
            signal.clone(),
            deadline,
        )
        .await?
        .map_err(map_reqwest_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = await_with_controls(response.bytes(), signal, deadline)
            .await?
            .map_err(map_reqwest_error)?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    async fn send_stream(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
        signal: Option<LlamaCancellation>,
        deadline: Option<Instant>,
    ) -> Result<reqwest::Response, LlamaError> {
        await_with_controls(
            self.request_builder(method, url, body.as_ref()).send(),
            signal,
            deadline,
        )
        .await?
        .map_err(map_reqwest_error)
    }
}

fn map_reqwest_error(error: reqwest::Error) -> LlamaError {
    if error.is_timeout() {
        LlamaError::Timeout
    } else {
        LlamaError::Transport(error.to_string())
    }
}

async fn await_with_controls<F, T>(
    future: F,
    signal: Option<LlamaCancellation>,
    deadline: Option<Instant>,
) -> Result<T, LlamaError>
where
    F: Future<Output = T>,
{
    if signal
        .as_ref()
        .is_some_and(|value| value.load(Ordering::SeqCst))
    {
        return Err(LlamaError::Cancelled);
    }
    if deadline.is_some_and(|value| Instant::now() >= value) {
        return Err(LlamaError::Timeout);
    }

    tokio::pin!(future);
    match (signal, deadline) {
        (None, None) => Ok(future.await),
        (Some(signal), None) => {
            tokio::select! {
                result = &mut future => Ok(result),
                _ = wait_for_cancellation(signal) => Err(LlamaError::Cancelled),
            }
        }
        (None, Some(deadline)) => {
            let timer = tokio::time::sleep(deadline.saturating_duration_since(Instant::now()));
            tokio::pin!(timer);
            tokio::select! {
                result = &mut future => Ok(result),
                _ = &mut timer => Err(LlamaError::Timeout),
            }
        }
        (Some(signal), Some(deadline)) => {
            let timer = tokio::time::sleep(deadline.saturating_duration_since(Instant::now()));
            tokio::pin!(timer);
            tokio::select! {
                result = &mut future => Ok(result),
                _ = wait_for_cancellation(signal) => Err(LlamaError::Cancelled),
                _ = &mut timer => Err(LlamaError::Timeout),
            }
        }
    }
}

async fn wait_for_cancellation(signal: LlamaCancellation) {
    while !signal.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn sleep_with_controls(
    duration: Duration,
    signal: Option<LlamaCancellation>,
    deadline: Instant,
) -> Result<(), LlamaError> {
    let until = Instant::now() + duration;
    let end = until.min(deadline);
    if Instant::now() >= end {
        return if deadline <= until {
            Err(LlamaError::Timeout)
        } else {
            Ok(())
        };
    }
    let timer = tokio::time::sleep(end.saturating_duration_since(Instant::now()));
    tokio::pin!(timer);
    match signal {
        Some(signal) => {
            tokio::select! {
                _ = &mut timer => if deadline <= until { Err(LlamaError::Timeout) } else { Ok(()) },
                _ = wait_for_cancellation(signal) => Err(LlamaError::Cancelled),
            }
        }
        None => {
            timer.await;
            if deadline <= until {
                Err(LlamaError::Timeout)
            } else {
                Ok(())
            }
        }
    }
}

pub fn normalize_llama_server_url(value: &str) -> Result<String, LlamaError> {
    let value = value.trim();
    let mut url =
        Url::parse(value).map_err(|error| LlamaError::InvalidServerUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(LlamaError::InvalidScheme);
    }
    if url.host_str().is_none() {
        return Err(LlamaError::InvalidServerUrl(
            "server URL must include a host".to_owned(),
        ));
    }
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().trim_end_matches('/').to_owned();
    if path == "/v1" {
        path.clear();
    } else if path.ends_with("/v1") {
        path.truncate(path.len() - 3);
        path = path.trim_end_matches('/').to_owned();
    }
    url.set_path(if path.is_empty() { "/" } else { &path });
    let normalized = url.to_string();
    Ok(normalized.trim_end_matches('/').to_owned())
}

pub fn llama_inference_url(value: &str) -> Result<String, LlamaError> {
    Ok(format!("{}/v1", normalize_llama_server_url(value)?))
}

fn join_endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn parse_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn error_message(payload: &Value, fallback: String) -> String {
    payload
        .get("error")
        .and_then(|error| error.get("message").and_then(Value::as_str))
        .filter(|message| !message.is_empty())
        .unwrap_or(&fallback)
        .to_owned()
}

fn checked_ratio(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = units[0];
    for candidate in units {
        value /= 1024.0;
        unit = candidate;
        if value < 1024.0 || candidate == "TiB" {
            break;
        }
    }
    if value >= 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}

pub fn validate_model_catalog(models: &[LlamaModelInfo]) -> Result<(), LlamaError> {
    let mut ids = HashMap::with_capacity(models.len());
    for model in models {
        if model.id.trim().is_empty() {
            return Err(LlamaError::InvalidCatalog("model id is empty".to_owned()));
        }
        if ids.insert(model.id.clone(), ()).is_some() {
            return Err(LlamaError::InvalidCatalog(format!(
                "duplicate model id: {}",
                model.id
            )));
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct LlamaClient {
    server_url: String,
    http: HttpClient,
}

impl LlamaClient {
    pub fn new(server_url: impl AsRef<str>, api_key: Option<&str>) -> Result<Self, LlamaError> {
        Self::with_timeout(server_url, api_key, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn with_timeout(
        server_url: impl AsRef<str>,
        api_key: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, LlamaError> {
        let server_url = normalize_llama_server_url(server_url.as_ref())?;
        let http = HttpClient::new(timeout, api_key)?;
        Ok(Self { server_url, http })
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn inference_url(&self) -> String {
        format!("{}/v1", self.server_url)
    }

    async fn request_json(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        signal: Option<LlamaCancellation>,
        deadline: Option<Instant>,
    ) -> Result<Value, LlamaError> {
        let response = self
            .http
            .send(
                method,
                &join_endpoint(&self.server_url, path),
                body,
                signal,
                deadline,
            )
            .await?;
        let payload = parse_json(&response.body);
        if !response.status.is_success() {
            return Err(LlamaError::Http {
                status: response.status.as_u16(),
                message: error_message(
                    &payload,
                    format!("llama.cpp returned HTTP {}", response.status.as_u16()),
                ),
            });
        }
        Ok(payload)
    }

    async fn list_at(
        &self,
        reload: bool,
        signal: Option<LlamaCancellation>,
        deadline: Option<Instant>,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let path = if reload {
            "/models?reload=1"
        } else {
            "/models"
        };
        let payload = self
            .request_json(Method::GET, path, None, signal, deadline)
            .await?;
        let Some(data) = payload.get("data").and_then(Value::as_array) else {
            return Err(LlamaError::InvalidCatalog(
                "response did not contain a data array".to_owned(),
            ));
        };
        let mut models = Vec::with_capacity(data.len());
        for value in data {
            let model = serde_json::from_value::<LlamaModelInfo>(value.clone())
                .map_err(|_| LlamaError::NotRouterMode)?;
            models.push(model);
        }
        validate_model_catalog(&models)?;
        Ok(models)
    }

    pub async fn list(&self, options: LlamaListOptions) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        self.list_at(options.reload, options.signal, None).await
    }

    async fn post_model(
        &self,
        path: &str,
        model: &str,
        signal: Option<LlamaCancellation>,
        deadline: Option<Instant>,
    ) -> Result<Value, LlamaError> {
        self.request_json(
            Method::POST,
            path,
            Some(json!({"model": model})),
            signal,
            deadline,
        )
        .await
    }

    pub async fn load(
        &self,
        model: &str,
        signal: Option<LlamaCancellation>,
    ) -> Result<(), LlamaError> {
        self.post_model("/models/load", model, signal, None)
            .await
            .map(|_| ())
    }

    pub async fn unload(
        &self,
        model: &str,
        signal: Option<LlamaCancellation>,
    ) -> Result<(), LlamaError> {
        self.post_model("/models/unload", model, signal, None)
            .await
            .map(|_| ())
    }

    pub async fn download(
        &self,
        model: &str,
        signal: Option<LlamaCancellation>,
    ) -> Result<(), LlamaError> {
        self.post_model("/models", model, signal, None)
            .await
            .map(|_| ())
    }

    pub async fn unload_and_wait(
        &self,
        model: &str,
        options: LlamaWaitOptions,
    ) -> Result<(), LlamaError> {
        let deadline = Instant::now() + options.timeout;
        self.post_model(
            "/models/unload",
            model,
            options.signal.clone(),
            Some(deadline),
        )
        .await?;
        loop {
            let models = self
                .list_at(false, options.signal.clone(), Some(deadline))
                .await?;
            match models.iter().find(|entry| entry.id == model) {
                None => return Ok(()),
                Some(entry) if entry.status.value == LlamaModelStatusValue::Unloaded => {
                    return Ok(())
                }
                Some(_) => {
                    sleep_with_controls(options.poll_interval, options.signal.clone(), deadline)
                        .await?
                }
            }
        }
    }

    pub async fn watch<F>(
        &self,
        signal: Option<LlamaCancellation>,
        mut on_event: F,
    ) -> Result<(), LlamaError>
    where
        F: FnMut(LlamaModelEvent),
    {
        let response = self
            .http
            .send_stream(
                Method::GET,
                &join_endpoint(&self.server_url, "/models/sse"),
                None,
                signal.clone(),
                None,
            )
            .await?;
        if !response.status().is_success() {
            return Err(LlamaError::SseHttp {
                status: response.status().as_u16(),
            });
        }
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = await_with_controls(stream.next(), signal.clone(), None).await? {
            let chunk = chunk.map_err(map_reqwest_error)?;
            parse_sse_bytes(&mut buffer, &chunk, &mut on_event);
        }
        if !buffer.is_empty() {
            parse_sse_frame(&buffer, &mut on_event);
        }
        if signal.is_some_and(|value| value.load(Ordering::SeqCst)) {
            return Err(LlamaError::Cancelled);
        }
        Ok(())
    }

    pub async fn load_and_wait<F>(
        &self,
        model: &str,
        options: LlamaWaitOptions,
        mut on_progress: F,
    ) -> Result<LlamaModelInfo, LlamaError>
    where
        F: FnMut(LlamaProgress),
    {
        let deadline = Instant::now() + options.timeout;
        self.transition_and_wait(model, options, false, deadline, &mut on_progress)
            .await
    }

    pub async fn download_and_wait<F>(
        &self,
        model: &str,
        options: LlamaWaitOptions,
        mut on_progress: F,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError>
    where
        F: FnMut(LlamaProgress),
    {
        let final_signal = options.signal.clone();
        let final_deadline = Instant::now() + options.timeout;
        self.transition_and_wait(model, options, true, final_deadline, &mut on_progress)
            .await
            .map(|_| ())?;
        self.list_at(true, final_signal, Some(final_deadline)).await
    }

    async fn transition_and_wait<F>(
        &self,
        model: &str,
        options: LlamaWaitOptions,
        download: bool,
        deadline: Instant,
        on_progress: &mut F,
    ) -> Result<LlamaModelInfo, LlamaError>
    where
        F: FnMut(LlamaProgress),
    {
        let watcher_signal = Arc::new(AtomicBool::new(false));
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let watcher_client = self.clone();
        let watcher_signal_for_task = watcher_signal.clone();
        let watcher = tokio::spawn(async move {
            let _ = watcher_client
                .watch(Some(watcher_signal_for_task), move |event| {
                    let _ = events_tx.send(event);
                })
                .await;
        });

        let result = async {
            on_progress(LlamaProgress {
                message: if download {
                    "Downloading model".to_owned()
                } else {
                    "Loading model".to_owned()
                },
                ratio: None,
                detail: None,
            });
            self.post_model(
                if download { "/models" } else { "/models/load" },
                model,
                options.signal.clone(),
                Some(deadline),
            )
            .await?;

            let mut saw_download_finished = false;
            let mut saw_loaded_event = false;
            let mut download_polls = 0_u8;
            let mut load_polls = 0_u8;
            let mut terminal_grace_at = None;
            loop {
                while let Ok(event) = events_rx.try_recv() {
                    consume_transition_event(
                        event,
                        model,
                        download,
                        &mut saw_download_finished,
                        &mut saw_loaded_event,
                        on_progress,
                    )?;
                }

                let models = self
                    .list_at(false, options.signal.clone(), Some(deadline))
                    .await?;
                if download {
                    download_polls = download_polls.saturating_add(1);
                } else {
                    load_polls = load_polls.saturating_add(1);
                }
                if let Some(entry) = models.iter().find(|entry| entry.id == model) {
                    if entry.status.failed == Some(true) {
                        return Err(LlamaError::OperationFailed(if download {
                            "Download failed".to_owned()
                        } else {
                            "Model failed to load".to_owned()
                        }));
                    }
                    if let Some(exit_code) = entry.status.exit_code {
                        return Err(LlamaError::OperationFailed(format!(
                            "Model exited with code {exit_code}"
                        )));
                    }
                    if !download
                        && entry.status.value == LlamaModelStatusValue::Loaded
                        && (saw_loaded_event || load_polls >= 2)
                    {
                        if !saw_loaded_event {
                            let since = terminal_grace_at.get_or_insert_with(Instant::now);
                            if since.elapsed() < Duration::from_millis(50) {
                                sleep_with_controls(
                                    Duration::from_millis(10),
                                    options.signal.clone(),
                                    deadline,
                                )
                                .await?;
                                continue;
                            }
                        }
                        return Ok(entry.clone());
                    }
                    if download
                        && !matches!(entry.status.value, LlamaModelStatusValue::Downloading)
                        && (saw_download_finished || download_polls >= 2)
                    {
                        if !saw_download_finished {
                            let since = terminal_grace_at.get_or_insert_with(Instant::now);
                            if since.elapsed() < Duration::from_millis(50) {
                                sleep_with_controls(
                                    Duration::from_millis(10),
                                    options.signal.clone(),
                                    deadline,
                                )
                                .await?;
                                continue;
                            }
                        }
                        // The SSE watcher runs on a separate task. Let it make
                        // one scheduler turn and drain events that arrived
                        // with the terminal catalog response before tearing it
                        // down. This preserves progress callbacks even when a
                        // server closes the stream immediately after
                        // `download_finished`.
                        tokio::task::yield_now().await;
                        while let Ok(event) = events_rx.try_recv() {
                            consume_transition_event(
                                event,
                                model,
                                true,
                                &mut saw_download_finished,
                                &mut saw_loaded_event,
                                on_progress,
                            )?;
                        }
                        return Ok(entry.clone());
                    }
                } else if download && saw_download_finished {
                    return Ok(synthetic_download_model(model));
                } else if !download && saw_loaded_event {
                    return Ok(synthetic_loaded_model(model));
                }

                sleep_with_controls(options.poll_interval, options.signal.clone(), deadline)
                    .await?;
            }
        }
        .await;

        watcher_signal.store(true, Ordering::SeqCst);
        let _ = watcher.await;
        result
    }
}

fn synthetic_loaded_model(model: &str) -> LlamaModelInfo {
    LlamaModelInfo {
        id: model.to_owned(),
        aliases: Vec::new(),
        status: LlamaModelStatus {
            value: LlamaModelStatusValue::Loaded,
            args: None,
            failed: None,
            exit_code: None,
            progress: None,
        },
        architecture: None,
        source: None,
        meta: None,
    }
}

fn synthetic_download_model(model: &str) -> LlamaModelInfo {
    LlamaModelInfo {
        id: model.to_owned(),
        aliases: Vec::new(),
        status: LlamaModelStatus {
            value: LlamaModelStatusValue::Unloaded,
            args: None,
            failed: None,
            exit_code: None,
            progress: None,
        },
        architecture: None,
        source: None,
        meta: None,
    }
}

fn event_message(event: &LlamaModelEvent, fallback: &str) -> String {
    let message = event
        .data
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .or_else(|| {
            event
                .data
                .as_ref()
                .and_then(|value| value.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
        });
    message.unwrap_or(fallback).to_owned()
}

fn consume_transition_event<F>(
    event: LlamaModelEvent,
    model: &str,
    download: bool,
    saw_download_finished: &mut bool,
    saw_loaded_event: &mut bool,
    on_progress: &mut F,
) -> Result<(), LlamaError>
where
    F: FnMut(LlamaProgress),
{
    if event.model != model {
        return Ok(());
    }
    if download {
        if event.event == "download_failed" {
            return Err(LlamaError::OperationFailed(event_message(
                &event,
                "Download failed",
            )));
        }
        if event.event == "download_finished" {
            *saw_download_finished = true;
        }
        if event.event == "download_progress" {
            if let Some(progress) = parse_download_progress(event.data.as_ref()) {
                on_progress(progress);
            }
        }
        return Ok(());
    }

    if !matches!(event.event.as_str(), "model_status" | "status_change") {
        return Ok(());
    }
    match event
        .data
        .as_ref()
        .and_then(|data| data.get("status"))
        .and_then(Value::as_str)
    {
        Some("loaded") => *saw_loaded_event = true,
        Some("unloaded") => {
            return Err(LlamaError::OperationFailed(
                "Model failed to load".to_owned(),
            ));
        }
        _ => {}
    }
    if let Some(progress) = parse_load_progress(event.data.as_ref()) {
        on_progress(progress);
    }
    Ok(())
}

pub fn parse_load_progress(data: Option<&Value>) -> Option<LlamaProgress> {
    let progress = data?.get("progress")?.as_object()?;
    let current = progress
        .get("current")
        .or_else(|| progress.get("stage"))
        .and_then(Value::as_str);
    let stages = progress
        .get("stages")
        .and_then(Value::as_array)
        .map(|stages| stages.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let stage_ratio = progress
        .get("value")
        .and_then(Value::as_f64)
        .map(checked_ratio);
    let ratio = match (current, stages.is_empty()) {
        (Some(current), false) => {
            let index = stages
                .iter()
                .position(|stage| *stage == current)
                .unwrap_or(0);
            Some(checked_ratio(
                (index as f64 + stage_ratio.unwrap_or(0.0)) / stages.len() as f64,
            ))
        }
        _ => stage_ratio,
    };
    Some(LlamaProgress {
        message: current
            .map(|current| format!("Loading {}", current.replace('_', " ")))
            .unwrap_or_else(|| "Loading model".to_owned()),
        ratio,
        detail: None,
    })
}

pub fn parse_download_progress(data: Option<&Value>) -> Option<LlamaProgress> {
    let data = data?;
    let progress = data
        .get("progress")
        .and_then(Value::as_object)
        .or_else(|| data.as_object())?;
    let mut done = 0_u64;
    let mut total = 0_u64;
    for value in progress.values() {
        let Some(value) = value.as_object() else {
            continue;
        };
        let Some(entry_done) = value.get("done").and_then(Value::as_u64) else {
            continue;
        };
        let Some(entry_total) = value.get("total").and_then(Value::as_u64) else {
            continue;
        };
        done = done.saturating_add(entry_done);
        total = total.saturating_add(entry_total);
    }
    if total == 0 {
        return None;
    }
    let ratio = checked_ratio(done as f64 / total as f64);
    Some(LlamaProgress {
        message: "Downloading model".to_owned(),
        ratio: Some(ratio),
        detail: Some(format!("{} / {}", format_bytes(done), format_bytes(total))),
    })
}

fn parse_sse_bytes<F>(buffer: &mut Vec<u8>, bytes: &[u8], on_event: &mut F)
where
    F: FnMut(LlamaModelEvent),
{
    buffer.extend_from_slice(bytes);
    loop {
        let lf = buffer.windows(2).position(|window| window == b"\n\n");
        let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
        let boundary = match (lf, crlf) {
            (Some(lf), Some(crlf)) if crlf < lf => Some((crlf, 4)),
            (Some(lf), _) => Some((lf, 2)),
            (None, Some(crlf)) => Some((crlf, 4)),
            (None, None) => None,
        };
        let Some((end, separator_len)) = boundary else {
            break;
        };
        let frame = buffer.drain(..end).collect::<Vec<_>>();
        buffer.drain(..separator_len);
        parse_sse_frame(&frame, on_event);
    }
}

fn parse_sse_frame<F>(frame: &[u8], on_event: &mut F)
where
    F: FnMut(LlamaModelEvent),
{
    let mut data = String::new();
    for line in frame.split(|byte| *byte == b'\n' || *byte == b'\r') {
        let Some(line) = line.strip_prefix(b"data:") else {
            continue;
        };
        let line = line.strip_prefix(b" ").unwrap_or(line);
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(&String::from_utf8_lossy(line));
    }
    if let Ok(event) = serde_json::from_str::<LlamaModelEvent>(&data) {
        if !event.model.trim().is_empty() && !event.event.trim().is_empty() {
            on_event(event);
        }
    }
}

pub fn llama_credential(
    server_url: impl AsRef<str>,
    api_key: Option<String>,
) -> Result<Credential, LlamaError> {
    let server_url = normalize_llama_server_url(server_url.as_ref())?;
    let mut env = ProviderEnv::new();
    env.insert("LLAMA_BASE_URL".to_owned(), server_url);
    Ok(Credential::ApiKey(ApiKeyCredential {
        key: api_key,
        env: Some(env),
    }))
}

#[derive(Clone)]
pub struct LlamaApiKeyAuth;

impl LlamaApiKeyAuth {
    fn server_url(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<String> {
        let stored = credential
            .and_then(|value| value.env.as_ref())
            .and_then(|value| value.get("LLAMA_BASE_URL"))
            .cloned()
            .filter(|value| !value.trim().is_empty());
        let configured = stored.or_else(|| {
            ctx.env("LLAMA_BASE_URL")
                .filter(|value| !value.trim().is_empty())
        });
        configured.and_then(|value| normalize_llama_server_url(&value).ok())
    }
}

fn login_inputs(interaction: &dyn AuthInteraction) -> Result<(String, Option<String>), String> {
    let default_url = env::var("LLAMA_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LLAMA_SERVER_URL.to_owned());
    let entered_url = interaction.prompt(&AuthPrompt::Text {
        message: "llama.cpp server URL".to_owned(),
        placeholder: Some(default_url.clone()),
    })?;
    let server_url = normalize_llama_server_url(if entered_url.trim().is_empty() {
        &default_url
    } else {
        entered_url.trim()
    })
    .map_err(|error| error.to_string())?;
    let entered_key = interaction.prompt(&AuthPrompt::Secret {
        message: "API key (optional)".to_owned(),
        placeholder: None,
    })?;
    let api_key = (!entered_key.trim().is_empty()).then(|| entered_key.trim().to_owned());
    Ok((server_url, api_key))
}

async fn validate_login(
    server_url: String,
    api_key: Option<String>,
    signal: Option<LlamaCancellation>,
) -> Result<ApiKeyCredential, String> {
    let client =
        LlamaClient::new(&server_url, api_key.as_deref()).map_err(|error| error.to_string())?;
    client
        .list(LlamaListOptions {
            reload: false,
            signal,
        })
        .await
        .map_err(|error| error.to_string())?;
    let mut env = ProviderEnv::new();
    env.insert("LLAMA_BASE_URL".to_owned(), server_url);
    Ok(ApiKeyCredential {
        key: api_key,
        env: Some(env),
    })
}

fn validate_login_sync(
    server_url: String,
    api_key: Option<String>,
    signal: Option<LlamaCancellation>,
) -> Result<ApiKeyCredential, String> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|error| format!("failed to start llama.cpp login runtime: {error}"))?;
        runtime.block_on(validate_login(server_url, api_key, signal))
    })
    .join()
    .map_err(|_| "llama.cpp login validation thread panicked".to_owned())?
}

impl LlamaApiKeyAuth {
    /// Run the upstream API-key login flow and validate the entered server
    /// through its real router catalog endpoint.
    pub fn login(&self, interaction: &dyn AuthInteraction) -> Result<ApiKeyCredential, PiAiError> {
        let (server_url, api_key) = login_inputs(interaction)?;
        validate_login_sync(server_url, api_key, interaction.signal()).map_err(PiAiError::from)
    }

    /// Async counterpart for hosts that already own a Tokio runtime.
    pub async fn login_async(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<ApiKeyCredential, String> {
        let (server_url, api_key) = login_inputs(interaction)?;
        validate_login(server_url, api_key, interaction.signal()).await
    }
}

impl ApiKeyAuth for LlamaApiKeyAuth {
    fn name(&self) -> &str {
        "llama.cpp server"
    }

    fn login(&self, interaction: &dyn AuthInteraction) -> Result<ApiKeyCredential, PiAiError> {
        LlamaApiKeyAuth::login(self, interaction)
    }

    fn check(&self, ctx: &AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthCheck> {
        self.server_url(ctx, credential).map(|_| AuthCheck {
            source: credential
                .map(|_| "stored credential".to_owned())
                .or_else(|| Some("LLAMA_BASE_URL".to_owned())),
            auth_type: "api_key",
        })
    }

    fn resolve(
        &self,
        ctx: &AuthContext,
        credential: Option<&ApiKeyCredential>,
    ) -> Option<AuthResult> {
        let server_url = self.server_url(ctx, credential)?;
        let api_key = credential
            .and_then(|value| value.key.clone())
            .or_else(|| ctx.env("LLAMA_API_KEY"))
            .unwrap_or_else(|| "local".to_owned());
        let mut env = credential
            .and_then(|value| value.env.clone())
            .unwrap_or_default();
        env.insert("LLAMA_BASE_URL".to_owned(), server_url.clone());
        Some(AuthResult {
            auth: ModelAuth {
                api_key: Some(api_key),
                headers: None,
                base_url: Some(format!("{server_url}/v1")),
            },
            env: Some(env),
            source: credential
                .map(|_| "stored credential".to_owned())
                .or_else(|| Some("LLAMA_BASE_URL".to_owned())),
        })
    }
}

fn model_from_info(server_url: &str, info: &LlamaModelInfo) -> Result<Model, LlamaError> {
    let base_url = llama_inference_url(server_url)?;
    let context_window = info
        .meta
        .as_ref()
        .and_then(|meta| meta.n_ctx.or(meta.n_ctx_train))
        .filter(|value| *value > 0)
        .unwrap_or(128_000);
    let supports_image = info
        .architecture
        .as_ref()
        .and_then(|architecture| architecture.input_modalities.as_ref())
        .is_some_and(|modalities| modalities.iter().any(|value| value == "image"));
    let mut model = Model::new(
        info.id.clone(),
        info.id.clone(),
        "openai-completions",
        LLAMA_PROVIDER_ID,
    );
    model.base_url = base_url;
    model.reasoning = false;
    model.input = if supports_image {
        vec![ModelInput::Text, ModelInput::Image]
    } else {
        vec![ModelInput::Text]
    };
    model.context_window = context_window;
    model.max_tokens = context_window;
    model.cost = pi_ai::model::ModelCost {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
        ..Default::default()
    };
    model.compat = Some(json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "supportsUsageInStreaming": true,
        "supportsStrictMode": false,
        "maxTokensField": "max_tokens"
    }));
    model.authenticated = true;
    Ok(model)
}

fn llama_streams() -> ProviderStreams {
    let client = reqwest::Client::new();
    let stream = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  context: &pi_ai::types::Context,
                  options: Option<&StreamOptions>| {
                let api_key = options.and_then(|value| value.base.api_key.as_deref());
                let chat_options = pi_ai::api::openai_completions::OpenAIChatOptions {
                    base: options.cloned().unwrap_or_default(),
                    reasoning_effort: None,
                    tool_choice: None,
                    thinking_budgets: None,
                };
                pi_ai::api::openai_completions::stream(
                    model,
                    context,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    &chat_options,
                )
            },
        )
    };
    let stream_simple = {
        let client = client.clone();
        Arc::new(
            move |model: &Model,
                  context: &pi_ai::types::Context,
                  options: Option<&SimpleStreamOptions>| {
                let Some(options) = options else {
                    return pi_ai::event_stream::create_error_stream(
                        &model.api,
                        &model.provider,
                        &model.id,
                        "streamSimple requires options".to_owned(),
                    );
                };
                let api_key = options.base.base.api_key.as_deref();
                pi_ai::api::openai_completions::stream_simple(
                    model,
                    context,
                    client.clone(),
                    &model.base_url,
                    api_key,
                    options,
                )
            },
        )
    };
    ProviderStreams {
        stream,
        stream_simple,
        fetch_deferred: None,
        cancel_deferred: None,
    }
}

#[derive(Clone)]
pub struct LlamaProviderController {
    provider: Provider,
    catalog: Arc<RwLock<Vec<LlamaModelInfo>>>,
    dynamic_models: Arc<RwLock<Vec<Model>>>,
}

impl Default for LlamaProviderController {
    fn default() -> Self {
        Self::new()
    }
}

impl LlamaProviderController {
    pub fn new() -> Self {
        let catalog = Arc::new(RwLock::new(Vec::new()));
        let dynamic_models = Arc::new(RwLock::new(Vec::new()));
        let auth = Arc::new(LlamaApiKeyAuth);
        let provider = create_provider(CreateProviderOptions {
            id: LLAMA_PROVIDER_ID.to_owned(),
            name: Some("llama.cpp".to_owned()),
            base_url: Some(format!("{DEFAULT_LLAMA_SERVER_URL}/v1")),
            headers: None,
            auth: ProviderAuth {
                api_key: Some(auth),
                oauth: None,
            },
            models: Vec::new(),
            api: ProviderApiSpec::Single(llama_streams()),
            filter_models: None,
        });
        let provisional = Self {
            provider: provider.clone(),
            catalog: catalog.clone(),
            dynamic_models: dynamic_models.clone(),
        };
        let provider = provider.with_refresh_models_state(
            llama_refresh_function(&provisional),
            dynamic_models.clone(),
        );
        Self {
            provider,
            catalog,
            dynamic_models,
        }
    }

    pub fn provider(&self) -> Provider {
        self.provider.clone()
    }

    pub fn register_into(&self, models: &Models) {
        models.set_provider(self.provider.clone());
    }

    pub fn catalog(&self) -> Vec<LlamaModelInfo> {
        self.catalog
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn models(&self) -> Vec<Model> {
        self.dynamic_models
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn set_catalog(
        &self,
        catalog: Vec<LlamaModelInfo>,
        server_url: &str,
    ) -> Result<Vec<Model>, LlamaError> {
        validate_model_catalog(&catalog)?;
        let models = catalog
            .iter()
            .filter(|info| info.status.value.is_selectable())
            .map(|info| model_from_info(server_url, info))
            .collect::<Result<Vec<_>, _>>()?;
        self.provider.set_dynamic_models(models.clone());
        *self
            .catalog
            .write()
            .unwrap_or_else(|error| error.into_inner()) = catalog;
        *self
            .dynamic_models
            .write()
            .unwrap_or_else(|error| error.into_inner()) = models.clone();
        Ok(models)
    }

    pub async fn refresh_catalog(
        &self,
        client: &LlamaClient,
        reload: bool,
        signal: Option<LlamaCancellation>,
    ) -> Result<Vec<LlamaModelInfo>, LlamaError> {
        let catalog = client.list(LlamaListOptions { reload, signal }).await?;
        self.set_catalog(catalog.clone(), client.server_url())?;
        Ok(catalog)
    }

    pub fn selection_options(&self, catalog: &[LlamaModelInfo]) -> Vec<LlamaSelectionOption> {
        let mut options = catalog
            .iter()
            .map(|info| {
                let action = match info.status.value {
                    LlamaModelStatusValue::Unloaded => LlamaModelAction::Load,
                    LlamaModelStatusValue::Loaded | LlamaModelStatusValue::Sleeping => {
                        LlamaModelAction::Unload
                    }
                    LlamaModelStatusValue::Loading | LlamaModelStatusValue::Downloading => {
                        LlamaModelAction::Observe
                    }
                };
                (
                    if info.status.value == LlamaModelStatusValue::Loaded {
                        0_u8
                    } else {
                        1_u8
                    },
                    info.id.clone(),
                    LlamaSelectionOption {
                        label: info.id.clone(),
                        description: llama_model_description(info),
                        action: LlamaManagerAction::Model {
                            id: info.id.clone(),
                            action,
                        },
                    },
                )
            })
            .collect::<Vec<_>>();
        options.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let mut options = options
            .into_iter()
            .map(|(_, _, option)| option)
            .collect::<Vec<_>>();
        options.push(LlamaSelectionOption {
            label: "Download a model".to_owned(),
            description: "Search Hugging Face GGUF models".to_owned(),
            action: LlamaManagerAction::Download,
        });
        options.push(LlamaSelectionOption {
            label: "Close".to_owned(),
            description: String::new(),
            action: LlamaManagerAction::Close,
        });
        options
    }
}

fn stored_models_from_context(context: &RefreshModelsContext) -> Vec<Model> {
    context
        .stored
        .as_ref()
        .map(|stored| {
            stored
                .models
                .iter()
                .filter(|model| {
                    model.provider == LLAMA_PROVIDER_ID && model.api == "openai-completions"
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn refresh_credentials(credential: Option<&Credential>) -> Option<(String, Option<String>)> {
    let Credential::ApiKey(credential) = credential? else {
        return None;
    };
    let server_url = credential
        .env
        .as_ref()
        .and_then(|environment| environment.get("LLAMA_BASE_URL"))
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| normalize_llama_server_url(value).ok())?;
    Some((server_url, credential.key.clone()))
}

pub fn llama_refresh_function(
    controller: &LlamaProviderController,
) -> pi_ai::models::RefreshModelsFn {
    let controller = controller.clone();
    Arc::new(move |context: RefreshModelsContext| {
        let controller = controller.clone();
        Box::pin(async move {
            let cached = stored_models_from_context(&context);
            if context.stored.is_some() {
                let dynamic_models = controller.dynamic_models.clone();
                let cached_for_update = cached.clone();
                let published = context
                    .publish(ModelsPublication {
                        update: Some(Arc::new(move || {
                            *dynamic_models
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) =
                                cached_for_update.clone();
                        })),
                        persist: None,
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                if !published {
                    return Ok(());
                }
            }
            if !context.allow_network || context.aborted() {
                return Ok(());
            }
            let Some((server_url, api_key)) = refresh_credentials(context.credential.as_ref())
            else {
                return Ok(());
            };
            let client = LlamaClient::new(server_url, api_key.as_deref())
                .map_err(|error| error.to_string())?;
            let catalog = client
                .list(LlamaListOptions {
                    reload: context.force,
                    signal: Some(context.signal.clone()),
                })
                .await
                .map_err(|error| error.to_string())?;
            if context.aborted() {
                return Ok(());
            }
            validate_model_catalog(&catalog).map_err(|error| error.to_string())?;
            let models = catalog
                .iter()
                .filter(|info| info.status.value.is_selectable())
                .map(|info| model_from_info(client.server_url(), info))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            let dynamic_models = controller.dynamic_models.clone();
            let catalog_state = controller.catalog.clone();
            let catalog_for_update = catalog.clone();
            let models_for_update = models.clone();
            let checked_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let published = context
                .publish(ModelsPublication {
                    update: Some(Arc::new(move || {
                        *dynamic_models
                            .write()
                            .unwrap_or_else(|error| error.into_inner()) = models_for_update.clone();
                        *catalog_state
                            .write()
                            .unwrap_or_else(|error| error.into_inner()) =
                            catalog_for_update.clone();
                    })),
                    persist: Some(ModelsPersistence::Write(ModelsStoreEntry {
                        models,
                        last_modified: None,
                        checked_at: Some(checked_at),
                        etag: None,
                    })),
                })
                .await
                .map_err(|error| error.to_string())?;
            if !published {
                return Ok(());
            }
            Ok(())
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceModel {
    pub id: String,
    pub downloads: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuggingFaceGated {
    NotGated,
    Auto,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceQuantization {
    pub name: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceModelDetails {
    pub id: String,
    pub gated: HuggingFaceGated,
    pub quantizations: Vec<HuggingFaceQuantization>,
}

pub fn find_huggingface_token() -> Option<String> {
    let env_values = env::vars().collect::<BTreeMap<_, _>>();
    let home = crate::config::home_dir();
    find_huggingface_token_from(&env_values, home.as_deref())
}

pub fn find_huggingface_token_from(
    environment: &BTreeMap<String, String>,
    home: Option<&Path>,
) -> Option<String> {
    if let Some(token) = environment
        .get("HF_TOKEN")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Some(token.trim().to_owned());
    }
    let mut paths = Vec::new();
    if let Some(path) = environment.get("HF_TOKEN_PATH") {
        paths.push(PathBuf::from(path));
    }
    if let Some(path) = environment.get("HF_HOME") {
        paths.push(PathBuf::from(path).join("token"));
    }
    if let Some(path) = environment.get("XDG_CACHE_HOME") {
        paths.push(PathBuf::from(path).join("huggingface/token"));
    }
    if let Some(home) = home {
        paths.push(home.join(".cache/huggingface/token"));
    }
    paths.into_iter().find_map(|path| {
        std::fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

#[derive(Clone)]
pub struct HuggingFaceClient {
    base_url: String,
    http: HttpClient,
}

impl HuggingFaceClient {
    pub fn new(token: Option<&str>) -> Result<Self, LlamaError> {
        Self::with_base_url(token, DEFAULT_HUGGINGFACE_URL)
    }

    pub fn with_base_url(
        token: Option<&str>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, LlamaError> {
        let base_url = normalize_http_url(base_url.as_ref())?;
        let http = HttpClient::new(DEFAULT_REQUEST_TIMEOUT, token)?;
        Ok(Self { base_url, http })
    }

    async fn get_json(
        &self,
        path: &str,
        signal: Option<LlamaCancellation>,
    ) -> Result<Value, LlamaError> {
        let response = self
            .http
            .send(
                Method::GET,
                &join_endpoint(&self.base_url, path),
                None,
                signal,
                None,
            )
            .await?;
        let payload = parse_json(&response.body);
        if !response.status.is_success() {
            let retry = retry_after(&response.headers);
            let message = error_message(&payload, "Hugging Face request failed".to_owned());
            return Err(LlamaError::HuggingFaceHttp {
                status: response.status.as_u16(),
                message: retry.map_or(message.clone(), |seconds| {
                    format!("{message}; retry after {seconds}s")
                }),
            });
        }
        Ok(payload)
    }

    fn parse_search_payload(payload: &Value) -> Result<Vec<HuggingFaceModel>, LlamaError> {
        let values = payload.as_array().ok_or_else(|| {
            LlamaError::InvalidHuggingFaceResponse("search response was not an array".to_owned())
        })?;
        Ok(values
            .iter()
            .filter_map(|value| {
                let object = value.as_object()?;
                let id = object.get("id").and_then(Value::as_str)?.to_owned();
                let downloads = object
                    .get("downloads")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                Some(HuggingFaceModel { id, downloads })
            })
            .collect())
    }

    fn parse_details_payload(
        payload: &Value,
        model_id: &str,
    ) -> Result<HuggingFaceModelDetails, LlamaError> {
        let object = payload.as_object().ok_or_else(|| {
            LlamaError::InvalidHuggingFaceResponse("model details were not an object".to_owned())
        })?;
        let gated = match object.get("gated") {
            Some(Value::String(value)) if value == "auto" => HuggingFaceGated::Auto,
            Some(Value::String(value)) if value == "manual" => HuggingFaceGated::Manual,
            Some(Value::Bool(true)) => HuggingFaceGated::Auto,
            _ => HuggingFaceGated::NotGated,
        };
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(model_id)
            .to_owned();
        let mut grouped = BTreeMap::<String, (u64, bool)>::new();
        for sibling in object
            .get("siblings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(sibling) = sibling.as_object() else {
                continue;
            };
            let Some(rfilename) = sibling.get("rfilename").and_then(Value::as_str) else {
                continue;
            };
            let filename = rfilename.to_lowercase();
            if !filename.ends_with(".gguf") || filename.contains("mmproj") {
                continue;
            }
            let name = quantization_name(rfilename);
            let entry = grouped.entry(name).or_insert((0, true));
            match sibling.get("size").and_then(Value::as_u64) {
                Some(size) => entry.0 = entry.0.saturating_add(size),
                None => entry.1 = false,
            }
        }
        let mut quantizations = grouped
            .into_iter()
            .map(|(name, (size, complete))| HuggingFaceQuantization {
                name,
                size: complete.then_some(size),
            })
            .collect::<Vec<_>>();
        quantizations.sort_by(|left, right| {
            quantization_rank(&left.name)
                .cmp(&quantization_rank(&right.name))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(HuggingFaceModelDetails {
            id,
            gated,
            quantizations,
        })
    }

    pub async fn search(
        &self,
        query: &str,
        signal: Option<LlamaCancellation>,
    ) -> Result<Vec<HuggingFaceModel>, LlamaError> {
        let query = urlencoding(query);
        let payload = self
            .get_json(
                &format!(
                    "/api/models?search={query}&filter=gguf&sort=downloads&direction=-1&limit=20"
                ),
                signal,
            )
            .await?;
        // Upstream accepts a partially valid result page instead of turning
        // one malformed hit into a failed search.
        Self::parse_search_payload(&payload)
    }

    pub async fn details(
        &self,
        model_id: &str,
        signal: Option<LlamaCancellation>,
    ) -> Result<HuggingFaceModelDetails, LlamaError> {
        let encoded = model_id
            .split('/')
            .map(urlencoding)
            .collect::<Vec<_>>()
            .join("/");
        let payload = self
            .get_json(&format!("/api/models/{encoded}?blobs=true"), signal)
            .await?;
        Self::parse_details_payload(&payload, model_id)
    }
}

fn normalize_http_url(value: &str) -> Result<String, LlamaError> {
    let mut url = Url::parse(value.trim())
        .map_err(|error| LlamaError::InvalidServerUrl(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(LlamaError::InvalidScheme);
    }
    if url.host_str().is_none() {
        return Err(LlamaError::InvalidServerUrl(
            "server URL must include a host".to_owned(),
        ));
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn retry_after(headers: &header::HeaderMap) -> Option<u64> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    value
        .split(';')
        .find_map(|part| part.trim().strip_prefix("t=")?.parse::<u64>().ok())
}

fn urlencoding(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn quantization_name(filename: &str) -> String {
    let stem = filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .strip_suffix(".gguf")
        .unwrap_or(filename);
    let stem = stem.trim_end_matches(['-', '_']);
    let stem = if stem.len() > 15 {
        let suffix = &stem[stem.len() - 15..];
        let suffix_bytes = suffix.as_bytes();
        let is_shard = suffix_bytes[0] == b'-'
            && suffix_bytes[6] == b'-'
            && suffix_bytes[7..9] == *b"of"
            && suffix_bytes[9] == b'-'
            && suffix_bytes[1..6].iter().all(u8::is_ascii_digit)
            && suffix_bytes[10..15].iter().all(u8::is_ascii_digit);
        if is_shard {
            stem[..stem.len() - 15].trim_end_matches('-')
        } else {
            stem
        }
    } else {
        stem
    };
    let upper = stem.to_uppercase();
    for marker in ["UD-IQ", "UD-Q", "IQ", "Q", "BF16", "F16", "F32", "MXFP"] {
        if let Some(index) = upper.rfind(marker) {
            return stem[index..].to_owned();
        }
    }
    stem.to_owned()
}

fn quantization_rank(name: &str) -> usize {
    if name.eq_ignore_ascii_case("Q4_K_M") {
        0
    } else if name.eq_ignore_ascii_case("Q5_K_M") {
        1
    } else {
        2
    }
}

fn llama_context_label(info: &LlamaModelInfo) -> Option<String> {
    let context = info
        .meta
        .as_ref()
        .and_then(|meta| meta.n_ctx.or(meta.n_ctx_train));
    if let Some(context) = context.filter(|context| *context > 0) {
        return Some(if context >= 1000 {
            format!("{}k", context.saturating_add(500) / 1000)
        } else {
            context.to_string()
        });
    }
    info.status.args.as_ref().and_then(|args| {
        args.windows(2).find_map(|window| {
            if !matches!(window[0].as_str(), "--ctx-size" | "-c" | "-ctx") {
                return None;
            }
            window[1]
                .parse::<u64>()
                .ok()
                .filter(|context| *context > 0)
                .map(|context| {
                    if context >= 1000 {
                        format!("{}k", context.saturating_add(500) / 1000)
                    } else {
                        context.to_string()
                    }
                })
        })
    })
}

fn llama_model_description(info: &LlamaModelInfo) -> String {
    let loaded = info.status.value.is_loaded();
    let mut details = Vec::new();
    if loaded {
        details.push("loaded".to_owned());
        if let Some(context) = llama_context_label(info) {
            details.push(format!("{context} context"));
        }
    } else if info.status.value != LlamaModelStatusValue::Unloaded {
        details.push(
            serde_json::to_value(info.status.value)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| "transitioning".to_owned()),
        );
    }
    details.join(" · ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn load_progress_accepts_missing_stage_metadata() {
        let progress = parse_load_progress(Some(&json!({
            "progress": {"value": 0.25}
        })))
        .expect("progress object");
        assert_eq!(progress.message, "Loading model");
        assert_eq!(progress.ratio, Some(0.25));

        let progress = parse_load_progress(Some(&json!({
            "progress": {"stages": ["weights"], "value": 0.5}
        })))
        .expect("progress object");
        assert_eq!(progress.message, "Loading model");
        assert_eq!(progress.ratio, Some(0.5));
    }

    #[test]
    fn selection_options_match_loaded_first_and_context_descriptions() {
        let controller = LlamaProviderController::new();
        let catalog = vec![
            LlamaModelInfo {
                id: "sleeping.gguf".to_owned(),
                aliases: Vec::new(),
                status: LlamaModelStatus {
                    value: LlamaModelStatusValue::Sleeping,
                    args: None,
                    failed: None,
                    exit_code: None,
                    progress: None,
                },
                architecture: None,
                source: None,
                meta: Some(LlamaModelMeta {
                    n_ctx: None,
                    n_ctx_train: Some(8192),
                    size: None,
                    ftype: None,
                }),
            },
            LlamaModelInfo {
                id: "loaded.gguf".to_owned(),
                aliases: Vec::new(),
                status: LlamaModelStatus {
                    value: LlamaModelStatusValue::Loaded,
                    args: None,
                    failed: None,
                    exit_code: None,
                    progress: None,
                },
                architecture: None,
                source: None,
                meta: Some(LlamaModelMeta {
                    n_ctx: Some(65536),
                    n_ctx_train: None,
                    size: None,
                    ftype: None,
                }),
            },
        ];
        let options = controller.selection_options(&catalog);
        assert_eq!(options[0].label, "loaded.gguf");
        assert_eq!(options[0].description, "loaded · 66k context");
        assert_eq!(options[1].label, "sleeping.gguf");
        assert_eq!(options[1].description, "loaded · 8k context");
    }

    #[test]
    fn huggingface_search_and_details_skip_malformed_records() {
        let search = HuggingFaceClient::parse_search_payload(&json!([
            {"id": "org/model", "downloads": 7},
            {"id": 42, "downloads": 99},
            "not-an-object",
            {"id": "org/other", "downloads": "unknown"}
        ]))
        .expect("array response");
        assert_eq!(
            search,
            vec![
                HuggingFaceModel {
                    id: "org/model".to_owned(),
                    downloads: 7,
                },
                HuggingFaceModel {
                    id: "org/other".to_owned(),
                    downloads: 0,
                },
            ]
        );

        let details = HuggingFaceClient::parse_details_payload(
            &json!({
                "gated": "manual",
                "siblings": [
                    {"rfilename": "org-model.Q4_K_M.gguf", "size": 100},
                    {"rfilename": 12, "size": 200},
                    {"size": 300},
                    "not-an-object"
                ]
            }),
            "org/model",
        )
        .expect("object response");
        assert_eq!(details.id, "org/model");
        assert_eq!(details.gated, HuggingFaceGated::Manual);
        assert_eq!(details.quantizations[0].name, "Q4_K_M");
        assert_eq!(details.quantizations[0].size, Some(100));
    }

    #[test]
    fn refresh_credentials_requires_stored_api_key_url() {
        let mut environment = ProviderEnv::new();
        environment.insert(
            "LLAMA_BASE_URL".to_owned(),
            "http://127.0.0.1:8080/v1/".to_owned(),
        );
        let credential = Credential::ApiKey(ApiKeyCredential {
            key: Some("local-secret".to_owned()),
            env: Some(environment),
        });

        assert_eq!(
            refresh_credentials(Some(&credential)),
            Some((
                "http://127.0.0.1:8080".to_owned(),
                Some("local-secret".to_owned())
            ))
        );
        assert!(refresh_credentials(None).is_none());
        assert!(
            refresh_credentials(Some(&Credential::ApiKey(ApiKeyCredential {
                key: Some("ambient-must-not-refresh".to_owned()),
                env: None,
            },)))
            .is_none()
        );
    }
    #[test]
    fn normalize_llama_server_url_matches_upstream_shapes() {
        assert_eq!(
            normalize_llama_server_url("http://127.0.0.1:8080/").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_llama_server_url("  http://127.0.0.1:8080/v1/  ").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_llama_server_url("http://127.0.0.1:8080/v1?token=x#frag").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_llama_server_url("https://router.example:8080/prefix/v1").unwrap(),
            "https://router.example:8080/prefix"
        );
        assert_eq!(
            llama_inference_url("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080/v1"
        );
        assert!(normalize_llama_server_url("ftp://127.0.0.1:8080").is_err());
        assert!(normalize_llama_server_url("127.0.0.1:8080").is_err());
        assert!(normalize_llama_server_url("").is_err());
    }

    #[test]
    fn stored_credential_env_beats_context_env() {
        let ctx = AuthContext {
            env: std::sync::Arc::new(|name| {
                (name == "LLAMA_BASE_URL").then(|| "http://ctx.example:8080".to_owned())
            }),
            file_exists: std::sync::Arc::new(|_| false),
        };
        let auth = LlamaApiKeyAuth;

        let mut stored = ProviderEnv::new();
        stored.insert(
            "LLAMA_BASE_URL".to_owned(),
            "http://stored.example:8080/v1/".to_owned(),
        );
        let credential = ApiKeyCredential {
            key: None,
            env: Some(stored),
        };
        assert_eq!(
            auth.server_url(&ctx, Some(&credential)),
            Some("http://stored.example:8080".to_owned())
        );

        let mut blank = ProviderEnv::new();
        blank.insert("LLAMA_BASE_URL".to_owned(), "   ".to_owned());
        let blank_credential = ApiKeyCredential {
            key: None,
            env: Some(blank),
        };
        assert_eq!(
            auth.server_url(&ctx, Some(&blank_credential)),
            Some("http://ctx.example:8080".to_owned())
        );
        assert_eq!(
            auth.server_url(&ctx, None),
            Some("http://ctx.example:8080".to_owned())
        );
    }
    #[test]
    fn huggingface_token_search_follows_upstream_path_precedence() {
        let root = std::env::temp_dir().join(format!("pi-hf-token-{}", uuid::Uuid::new_v4()));
        let explicit = root.join("explicit-token");
        let hf_home = root.join("hf-home");
        let xdg_cache = root.join("xdg-cache");
        let home = root.join("home");
        for dir in [
            &hf_home,
            &xdg_cache.join("huggingface"),
            &home.join(".cache/huggingface"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&explicit, "explicit-secret\n").unwrap();
        std::fs::write(hf_home.join("token"), "hf-home-secret\n").unwrap();
        std::fs::write(xdg_cache.join("huggingface/token"), "xdg-secret\n").unwrap();
        std::fs::write(home.join(".cache/huggingface/token"), "home-secret\n").unwrap();

        let environment = BTreeMap::from([
            ("HF_TOKEN_PATH".to_owned(), explicit.display().to_string()),
            ("HF_HOME".to_owned(), hf_home.display().to_string()),
            ("XDG_CACHE_HOME".to_owned(), xdg_cache.display().to_string()),
        ]);
        assert_eq!(
            find_huggingface_token_from(&environment, Some(&home)),
            Some("explicit-secret".to_owned())
        );

        let environment = BTreeMap::from([
            ("HF_HOME".to_owned(), hf_home.display().to_string()),
            ("XDG_CACHE_HOME".to_owned(), xdg_cache.display().to_string()),
        ]);
        assert_eq!(
            find_huggingface_token_from(&environment, Some(&home)),
            Some("hf-home-secret".to_owned())
        );

        let environment =
            BTreeMap::from([("XDG_CACHE_HOME".to_owned(), xdg_cache.display().to_string())]);
        assert_eq!(
            find_huggingface_token_from(&environment, Some(&home)),
            Some("xdg-secret".to_owned())
        );
        assert_eq!(
            find_huggingface_token_from(&BTreeMap::new(), Some(&home)),
            Some("home-secret".to_owned())
        );
        assert_eq!(find_huggingface_token_from(&BTreeMap::new(), None), None);

        let environment = BTreeMap::from([("HF_TOKEN".to_owned(), "  env-secret  ".to_owned())]);
        assert_eq!(
            find_huggingface_token_from(&environment, Some(&home)),
            Some("env-secret".to_owned())
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
