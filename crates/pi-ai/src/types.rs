//! Core message/model/stream types — port of `packages/ai/src/types.ts`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{atomic::AtomicBool, Arc};

/// JSON-compatible value (mirrors `JsonValue` in the upstream types).
pub type JsonValue = serde_json::Value;

/// Shared abort flag for provider streams.
///
/// The upstream API exposes an `AbortSignal`.  The Rust runtime already uses
/// an `Arc<AtomicBool>` for cooperative cancellation, so stream options carry
/// that same signal without changing the assistant-message protocol.
pub type AbortSignal = Arc<AtomicBool>;

/// Rust representation of the upstream asynchronous `onPayload` hook.
///
/// The hook receives owned values so a caller can await work without holding
/// a borrow into the provider's request-building state. Returning `None`
/// preserves the generated payload; returning `Some` replaces it before wire
/// conversion or transport selection.
pub type OnPayloadFuture = Pin<Box<dyn Future<Output = Option<JsonValue>> + Send>>;
pub type OnPayloadFn = Arc<dyn Fn(JsonValue, crate::model::Model) -> OnPayloadFuture + Send + Sync>;

pub const KNOWN_APIS: [&str; 10] = [
    "openai-completions",
    "mistral-conversations",
    "openai-responses",
    "azure-openai-responses",
    "openai-codex-responses",
    "anthropic-messages",
    "bedrock-converse-stream",
    "google-generative-ai",
    "google-vertex",
    "pi-messages",
];

pub type Api = String;
pub const KNOWN_IMAGES_APIS: [&str; 1] = ["openrouter-images"];
pub type ImagesApi = String;

pub const KNOWN_PROVIDERS: [&str; 41] = [
    "amazon-bedrock",
    "ant-ling",
    "anthropic",
    "google",
    "google-vertex",
    "openai",
    "azure-openai-responses",
    "openai-codex",
    "radius",
    "nvidia",
    "deepseek",
    "github-copilot",
    "xai",
    "groq",
    "cerebras",
    "openrouter",
    "vercel-ai-gateway",
    "zai",
    "zai-coding-cn",
    "mistral",
    "minimax",
    "minimax-cn",
    "moonshotai",
    "moonshotai-cn",
    "huggingface",
    "fireworks",
    "together",
    "baseten",
    "opencode",
    "opencode-go",
    "kimi-coding",
    "cloudflare-workers-ai",
    "cloudflare-ai-gateway",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "xiaomi",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-sgp",
    "faux",
];

pub type ProviderId = String;
pub const KNOWN_IMAGES_PROVIDERS: [&str; 1] = ["openrouter"];
pub type ImagesProviderId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// `ModelThinkingLevel`: `"off" | ThinkingLevel`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

impl ModelThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelThinkingLevel::Off => "off",
            ModelThinkingLevel::Minimal => "minimal",
            ModelThinkingLevel::Low => "low",
            ModelThinkingLevel::Medium => "medium",
            ModelThinkingLevel::High => "high",
            ModelThinkingLevel::Xhigh => "xhigh",
            ModelThinkingLevel::Max => "max",
        }
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "minimal" => ThinkingLevel::Minimal,
            "low" => ThinkingLevel::Low,
            "medium" => ThinkingLevel::Medium,
            "high" => ThinkingLevel::High,
            "xhigh" => ThinkingLevel::Xhigh,
            "max" => ThinkingLevel::Max,
            _ => return Err(format!("invalid thinking level {s:?}")),
        })
    }
}

impl std::str::FromStr for ModelThinkingLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "off" => ModelThinkingLevel::Off,
            _ => ModelThinkingLevel::from(ThinkingLevel::from_str(s)?),
        })
    }
}

impl ModelThinkingLevel {
    /// Parse a raw effort string ("off"/"minimal"/.../"max") into the level.
    pub fn from_effort_str(s: &str) -> ModelThinkingLevel {
        match s {
            "off" => ModelThinkingLevel::Off,
            "minimal" => ModelThinkingLevel::Minimal,
            "low" => ModelThinkingLevel::Low,
            "medium" => ModelThinkingLevel::Medium,
            "high" => ModelThinkingLevel::High,
            "xhigh" => ModelThinkingLevel::Xhigh,
            "max" => ModelThinkingLevel::Max,
            _ => ModelThinkingLevel::Medium,
        }
    }
}

impl From<ThinkingLevel> for ModelThinkingLevel {
    fn from(v: ThinkingLevel) -> Self {
        match v {
            ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
            ThinkingLevel::Low => ModelThinkingLevel::Low,
            ThinkingLevel::Medium => ModelThinkingLevel::Medium,
            ThinkingLevel::High => ModelThinkingLevel::High,
            ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
            ThinkingLevel::Max => ModelThinkingLevel::Max,
        }
    }
}

/// `ThinkingLevelMap`: partial map from ModelThinkingLevel to provider value.
pub type ThinkingLevelMap = BTreeMap<ModelThinkingLevel, Option<String>>;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ChatTemplateKwargValue {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
    Var {
        #[serde(rename = "$var")]
        var: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        omit_when_off: Option<bool>,
    },
}

pub type CacheRetention = String; // "none" | "short" | "long"
pub const CACHE_RETENTION_NONE: &str = "none";
pub const CACHE_RETENTION_SHORT: &str = "short";
pub const CACHE_RETENTION_LONG: &str = "long";

pub type Transport = String; // "sse" | "websocket" | "websocket-cached" | "auto"
pub const TRANSPORT_SSE: &str = "sse";
pub const TRANSPORT_WEBSOCKET: &str = "websocket";
pub const TRANSPORT_WEBSOCKET_CACHED: &str = "websocket-cached";
pub const TRANSPORT_AUTO: &str = "auto";

/// Provider-scoped environment overrides.
pub type ProviderEnv = BTreeMap<String, String>;
/// Provider headers. `None` suppresses a default header with the same name.
pub type ProviderHeaders = BTreeMap<String, Option<String>>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

/// Request options shared by provider requests.
#[derive(Clone, Default)]
pub struct ProviderRequestOptions {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    pub headers: Option<ProviderHeaders>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub telemetry_context: Option<pi_telemetry::InMemoryTelemetryContext>,
}

impl std::fmt::Debug for ProviderRequestOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRequestOptions")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field(
                "env_keys",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .field(
                "header_names",
                &self
                    .headers
                    .as_ref()
                    .map(|headers| headers.keys().collect::<Vec<_>>()),
            )
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("telemetry_context", &self.telemetry_context)
            .finish()
    }
}

/// Stream request options.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub base: ProviderRequestOptions,
    pub temperature: Option<f64>,
    pub sampling_params: Option<JsonValue>,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub metadata: Option<JsonValue>,
    /// Cooperative cancellation signal corresponding to upstream
    /// `StreamOptions.signal`.
    pub abort_signal: Option<AbortSignal>,
    /// Optional asynchronous payload replacement hook corresponding to
    /// upstream `StreamOptions.onPayload`.
    pub on_payload: Option<OnPayloadFn>,
    /// Optional callback invoked with the provider response before the body
    /// is consumed. Providers that do not produce HTTP responses call it with
    /// a synthetic `{status: 200, headers: {}}` (matching upstream faux).
    /// Optional callback invoked with the provider response before the body
    /// is consumed. Providers that do not produce HTTP responses call it with
    /// a synthetic `{status: 200, headers: {}}` (matching upstream faux).
    pub on_response: Option<crate::model::OnResponseFn>,
}

/// Simple (provider-neutral) stream options used by agent runtime.
#[derive(Clone, Default)]
pub struct SimpleStreamOptions {
    pub base: StreamOptions,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ThinkingLevel>,
    pub deferred: Option<DeferredOption>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeferredOption {
    Bool(bool),
    Window(DeferredWindow),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferredWindow {
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "24h")]
    H24,
}

/// Token budgets for each thinking level (token-based providers only).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThinkingBudgets {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "textSignature")]
        text_signature: Option<String>,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "thinkingSignature")]
        thinking_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: JsonValue,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "thoughtSignature")]
        thought_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }
    }
    pub fn thinking(thinking: impl Into<String>) -> Self {
        ContentBlock::Thinking {
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: None,
        }
    }
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        ContentBlock::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, arguments: JsonValue) -> Self {
        ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
            namespace: None,
        }
    }
    pub fn as_thinking(&self) -> Option<&ContentBlock> {
        match self {
            ContentBlock::Thinking { .. } => Some(self),
            _ => None,
        }
    }
    pub fn set_tool_call_id(&mut self, id: String) {
        if let ContentBlock::ToolCall { id: slot, .. } = self {
            *slot = id;
        }
    }
    pub fn clear_thought_signature(&mut self) {
        if let ContentBlock::ToolCall {
            thought_signature, ..
        } = self
        {
            *thought_signature = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Usage / cost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead", alias = "cache_read")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite", alias = "cache_write")]
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Provider-reported token counts are signed because session adjustment
    /// records can reconcile a previous report with negative deltas.
    pub input: i64,
    pub output: i64,
    #[serde(rename = "cacheRead")]
    pub cache_read: i64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "cacheWrite1h")]
    pub cache_write_1h: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<i64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
    pub cost: Cost,
}

impl Cost {
    /// Convert a flat cost struct to the tiered model cost representation.
    pub fn into_tiered(self) -> crate::model::ModelCost {
        crate::model::ModelCost {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            tiers: None,
        }
    }
}

impl Usage {
    pub fn input_only(input: i64) -> Self {
        Self {
            input,
            total_tokens: input,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::Pending => "pending",
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "toolUse",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
            StopReason::Deferred => "deferred",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeferredHandle {
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    pub api: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "pollAfterMs")]
    pub poll_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

fn empty_user_content_body() -> UserContentBody {
    UserContentBody::Blocks(Vec::new())
}

fn deserialize_optional_user_content_body<'de, D>(
    deserializer: D,
) -> Result<UserContentBody, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<UserContentBody> as serde::Deserialize>::deserialize(deserializer)
        .map(|content| content.unwrap_or_else(empty_user_content_body))
}

fn deserialize_optional_content_blocks<'de, D>(
    deserializer: D,
) -> Result<Vec<ContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<Vec<ContentBlock>> as serde::Deserialize>::deserialize(deserializer)
        .map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum UserContent {
    #[serde(rename = "user")]
    RoleUser {
        #[serde(
            default = "empty_user_content_body",
            deserialize_with = "deserialize_optional_user_content_body"
        )]
        content: UserContentBody,
        timestamp: u64,
    },
}

impl UserContent {
    pub fn string(content: impl Into<String>, timestamp: u64) -> Self {
        UserContent::RoleUser {
            content: UserContentBody::String(content.into()),
            timestamp,
        }
    }
    pub fn blocks(content: Vec<ContentBlock>, timestamp: u64) -> Self {
        UserContent::RoleUser {
            content: UserContentBody::Blocks(content),
            timestamp,
        }
    }
    pub fn timestamp(&self) -> u64 {
        match self {
            UserContent::RoleUser { timestamp, .. } => *timestamp,
        }
    }
    pub fn content(&self) -> &UserContentBody {
        match self {
            UserContent::RoleUser { content, .. } => content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum UserContentBody {
    String(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AssistantMessage {
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default, deserialize_with = "deserialize_optional_content_blocks")]
        content: Vec<ContentBlock>,
        api: Option<Api>,
        provider: Option<ProviderId>,
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "responseModel")]
        response_model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "responseId")]
        response_id: Option<String>,
        /// Redacted provider/runtime diagnostics for failures and recovery
        /// paths. This mirrors the optional upstream diagnostics array while
        /// keeping ordinary successful messages byte-compatible.
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
        usage: Option<Usage>,
        #[serde(rename = "stopReason")]
        stop_reason: Option<StopReason>,
        #[serde(skip_serializing_if = "Option::is_none")]
        deferred: Option<DeferredHandle>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "errorMessage")]
        error_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "rawStopReason")]
        raw_stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "endTurn")]
        end_turn: Option<bool>,
        timestamp: u64,
    },
}

impl AssistantMessage {
    pub fn new() -> Self {
        AssistantMessage::Assistant {
            content: Vec::new(),
            api: None,
            provider: None,
            model: None,
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: None,
            stop_reason: None,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
        }
    }
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            AssistantMessage::Assistant { content, .. } => content,
        }
    }
    pub fn content_mut(&mut self) -> &mut Vec<ContentBlock> {
        match self {
            AssistantMessage::Assistant { content, .. } => content,
        }
    }
    pub fn content_len(&self) -> usize {
        match self {
            AssistantMessage::Assistant { content, .. } => content.len(),
        }
    }
    pub fn usage_mut(&mut self) -> Option<&mut Usage> {
        match self {
            AssistantMessage::Assistant { usage, .. } => usage.as_mut(),
        }
    }
    pub fn timestamp(&self) -> u64 {
        match self {
            AssistantMessage::Assistant { timestamp, .. } => *timestamp,
        }
    }
    pub fn set_content(&mut self, content: Vec<ContentBlock>) {
        match self {
            AssistantMessage::Assistant { content: c, .. } => *c = content,
        }
    }
    pub fn with_timestamp(mut self, ts: u64) -> Self {
        let AssistantMessage::Assistant { timestamp, .. } = &mut self;
        *timestamp = ts;
        self
    }
    pub fn stop_reason(&self) -> Option<StopReason> {
        match self {
            AssistantMessage::Assistant { stop_reason, .. } => *stop_reason,
        }
    }
    pub fn set_stop_reason(&mut self, reason: StopReason) {
        let AssistantMessage::Assistant { stop_reason, .. } = self;
        *stop_reason = Some(reason);
    }
    pub fn error_message(&self) -> Option<&str> {
        match self {
            AssistantMessage::Assistant { error_message, .. } => error_message.as_deref(),
        }
    }
    pub fn set_error_message(&mut self, message: impl Into<String>) {
        match self {
            AssistantMessage::Assistant { error_message, .. } => {
                *error_message = Some(message.into());
            }
        }
    }
    pub fn usage(&self) -> Option<&Usage> {
        match self {
            AssistantMessage::Assistant { usage, .. } => usage.as_ref(),
        }
    }
    pub fn set_usage(&mut self, usage: Usage) {
        let AssistantMessage::Assistant { usage: slot, .. } = self;
        *slot = Some(usage);
    }
    pub fn response_id(&self) -> Option<&str> {
        match self {
            AssistantMessage::Assistant { response_id, .. } => response_id.as_deref(),
        }
    }
    pub fn set_response_id(&mut self, id: String) {
        let AssistantMessage::Assistant { response_id, .. } = self;
        *response_id = Some(id);
    }

    pub fn diagnostics(&self) -> Option<&[AssistantMessageDiagnostic]> {
        match self {
            AssistantMessage::Assistant { diagnostics, .. } => diagnostics.as_deref(),
        }
    }

    pub fn append_diagnostic(&mut self, diagnostic: AssistantMessageDiagnostic) {
        let AssistantMessage::Assistant { diagnostics, .. } = self;
        diagnostics.get_or_insert_with(Vec::new).push(diagnostic);
    }
    pub fn set_response_model(&mut self, model: String) {
        let AssistantMessage::Assistant { response_model, .. } = self;
        *response_model = Some(model);
    }
    pub fn raw_stop_reason(&self) -> Option<&str> {
        match self {
            AssistantMessage::Assistant {
                raw_stop_reason, ..
            } => raw_stop_reason.as_deref(),
        }
    }
    pub fn set_raw_stop_reason(&mut self, reason: String) {
        let AssistantMessage::Assistant {
            raw_stop_reason, ..
        } = self;
        *raw_stop_reason = Some(reason);
    }
    pub fn deferred(&self) -> Option<&DeferredHandle> {
        match self {
            AssistantMessage::Assistant { deferred, .. } => deferred.as_ref(),
        }
    }
    pub fn set_deferred(&mut self, handle: DeferredHandle) {
        let AssistantMessage::Assistant { deferred, .. } = self;
        *deferred = Some(handle);
    }
    pub fn api(&self) -> Option<&str> {
        match self {
            AssistantMessage::Assistant { api, .. } => api.as_deref(),
        }
    }
    pub fn provider(&self) -> Option<&str> {
        match self {
            AssistantMessage::Assistant { provider, .. } => provider.as_deref(),
        }
    }
    pub fn model(&self) -> Option<&str> {
        match self {
            AssistantMessage::Assistant { model, .. } => model.as_deref(),
        }
    }
    pub fn set_api_provider_model(&mut self, api: &str, provider: &str, model: &str) {
        let AssistantMessage::Assistant {
            api: a,
            provider: p,
            model: m,
            ..
        } = self;
        *a = Some(api.to_string());
        *p = Some(provider.to_string());
        *m = Some(model.to_string());
    }
}

/// Structured diagnostic error details used by provider/recovery paths.
/// Values are intentionally JSON-compatible so provider-specific numeric or
/// string error codes round-trip without being guessed or discarded.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticErrorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<JsonValue>,
}

/// Redacted provider/runtime diagnostic attached to an assistant message.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub diagnostic_type: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<BTreeMap<String, JsonValue>>,
}

impl AssistantMessageDiagnostic {
    pub fn new(diagnostic_type: impl Into<String>) -> Self {
        Self {
            diagnostic_type: diagnostic_type.into(),
            timestamp: now_ms(),
            error: None,
            details: None,
        }
    }
}

impl Default for AssistantMessage {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ToolResultMessage {
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(default, deserialize_with = "deserialize_optional_content_blocks")]
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "addedToolNames")]
        added_tool_names: Option<Vec<String>>,
        #[serde(rename = "isError")]
        is_error: bool,
        timestamp: u64,
    },
}

impl ToolResultMessage {
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Self {
        ToolResultMessage::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            details: None,
            usage: None,
            added_tool_names: None,
            is_error,
            timestamp: now_ms(),
        }
    }
    pub fn text(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::new(
            tool_call_id,
            tool_name,
            vec![ContentBlock::text(output)],
            is_error,
        )
    }

    /// Builder: attach tool usage, details, and an explicit timestamp.
    pub fn with_details_usage_timestamp(
        mut self,
        usage: Option<Usage>,
        details: Option<JsonValue>,
        timestamp: u64,
    ) -> Self {
        let ToolResultMessage::ToolResult {
            details: d,
            usage: u,
            timestamp: t,
            ..
        } = &mut self;
        *d = details;
        *u = usage;
        *t = timestamp;
        self
    }

    pub fn timestamp(&self) -> u64 {
        match self {
            ToolResultMessage::ToolResult { timestamp, .. } => *timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
// Boxing these variants would alter the public construction/matching surface
// throughout the agent crates without changing the serialized contract.
#[allow(clippy::large_enum_variant)]
pub enum Message {
    User(UserContent),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
    pub fn timestamp(&self) -> u64 {
        match self {
            Message::User(u) => u.timestamp(),
            Message::Assistant(a) => a.timestamp(),
            Message::ToolResult(t) => t.timestamp(),
        }
    }
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Message::Assistant(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_tool_result(&self) -> Option<&ToolResultMessage> {
        match self {
            Message::ToolResult(t) => Some(t),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssistantImages {
    pub api: ImagesApi,
    pub provider: ImagesProviderId,
    pub model: String,
    pub output: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "responseId")]
    pub response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    pub stop_reason: ImagesStopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "errorMessage")]
    pub error_message: Option<String>,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// Context / tools
// ---------------------------------------------------------------------------

fn deserialize_constrained_sampling<'de, D>(
    deserializer: D,
) -> Result<Option<ConstrainedSampling>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum WireConstrainedSampling {
        Disabled(bool),
        Config(ConstrainedSampling),
    }

    match <Option<WireConstrainedSampling> as serde::Deserialize>::deserialize(deserializer)? {
        None | Some(WireConstrainedSampling::Disabled(false)) => Ok(None),
        Some(WireConstrainedSampling::Disabled(true)) => Err(serde::de::Error::custom(
            "constrainedSampling must be false or a configuration object",
        )),
        Some(WireConstrainedSampling::Config(config)) => Ok(Some(config)),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema (TypeBox schema serialized) for the tool parameters.
    pub parameters: JsonValue,
    #[serde(
        rename = "constrainedSampling",
        alias = "constrained_sampling",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_constrained_sampling"
    )]
    pub constrained_sampling: Option<ConstrainedSampling>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSampling {
    JsonSchema { strict: StrictPreference },
    Grammar { variants: BTreeMap<String, String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrictPreference {
    Prefer,
    Require,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

// ---------------------------------------------------------------------------
// Stream protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ContentBlock,
        partial: AssistantMessage,
    },
    Done {
        reason: DoneReason,
        message: AssistantMessage,
    },
    Error {
        reason: ErrorReason,
        error_message: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    pub fn partial(&self) -> Option<&AssistantMessage> {
        match self {
            AssistantMessageEvent::Start { partial } => Some(partial),
            AssistantMessageEvent::TextStart { partial, .. } => Some(partial),
            AssistantMessageEvent::TextDelta { partial, .. } => Some(partial),
            AssistantMessageEvent::TextEnd { partial, .. } => Some(partial),
            AssistantMessageEvent::ThinkingStart { partial, .. } => Some(partial),
            AssistantMessageEvent::ThinkingDelta { partial, .. } => Some(partial),
            AssistantMessageEvent::ThinkingEnd { partial, .. } => Some(partial),
            AssistantMessageEvent::ToolCallStart { partial, .. } => Some(partial),
            AssistantMessageEvent::ToolCallDelta { partial, .. } => Some(partial),
            AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    Stop,
    Length,
    ToolUse,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorReason {
    Aborted,
    Error,
}

// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ToolResultMessage {
    pub fn tool_call_id(&self) -> &str {
        match self {
            ToolResultMessage::ToolResult { tool_call_id, .. } => tool_call_id,
        }
    }
    pub fn tool_name(&self) -> &str {
        match self {
            ToolResultMessage::ToolResult { tool_name, .. } => tool_name,
        }
    }
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            ToolResultMessage::ToolResult { content, .. } => content,
        }
    }
    pub fn is_error(&self) -> bool {
        match self {
            ToolResultMessage::ToolResult { is_error, .. } => *is_error,
        }
    }
    pub fn details(&self) -> Option<&JsonValue> {
        match self {
            ToolResultMessage::ToolResult { details, .. } => details.as_ref(),
        }
    }
    pub fn set_content(&mut self, content: Vec<ContentBlock>) {
        match self {
            ToolResultMessage::ToolResult { content: c, .. } => *c = content,
        }
    }
    pub fn set_tool_call_id(&mut self, id: String) {
        match self {
            ToolResultMessage::ToolResult { tool_call_id, .. } => *tool_call_id = id,
        }
    }
}

/// Convenience constructor for a tool with a JSON-schema parameter object.
pub fn json_tool(name: &str, description: &str, parameters: &serde_json::Value) -> Tool {
    Tool {
        name: name.to_string(),
        description: description.to_string(),
        parameters: parameters.clone(),
        constrained_sampling: None,
    }
}
