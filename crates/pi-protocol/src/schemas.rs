//! Protocol schemas and typed message models — port of
//! `packages/protocol/src/schemas.ts` (TypeBox strict objects: additional
//! properties rejected, string minLength 1, integer minimums enforced).

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

pub const PROTOCOL_VERSION: u64 = 1;

/// Strict-object validation result.
pub type VResult<T> = Result<T, String>;

fn require_object(v: &JsonValue) -> VResult<&serde_json::Map<String, JsonValue>> {
    match v {
        JsonValue::Object(map) => Ok(map),
        _ => Err("expected an object".to_string()),
    }
}

fn require_string(value: &JsonValue, field: &str, min_length: usize) -> VResult<String> {
    match value {
        JsonValue::String(s) if s.len() >= min_length => Ok(s.clone()),
        JsonValue::String(_) => Err(format!("{field} must have minLength {min_length}")),
        _ => Err(format!("{field} must be a string")),
    }
}

fn require_integer(value: &JsonValue, field: &str, minimum: i64) -> VResult<i64> {
    match value {
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= minimum {
                    Ok(i)
                } else {
                    Err(format!("{field} must be >= {minimum}"))
                }
            } else {
                Err(format!("{field} must be an integer"))
            }
        }
        _ => Err(format!("{field} must be an integer")),
    }
}

/// Collects unknown keys on a strict object.
fn strict_object(map: &serde_json::Map<String, JsonValue>, allowed: &[&str]) -> VResult<()> {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unexpected property {key:?}"));
        }
    }
    Ok(())
}

fn get<'a>(map: &'a serde_json::Map<String, JsonValue>, key: &str) -> VResult<&'a JsonValue> {
    map.get(key)
        .ok_or_else(|| format!("missing property {key:?}"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

pub fn parse_thinking_level(v: &JsonValue) -> VResult<ThinkingLevel> {
    Ok(match require_string(v, "thinkingLevel", 1)? {
        s if s == "off" => ThinkingLevel::Off,
        s if s == "minimal" => ThinkingLevel::Minimal,
        s if s == "low" => ThinkingLevel::Low,
        s if s == "medium" => ThinkingLevel::Medium,
        s if s == "high" => ThinkingLevel::High,
        s if s == "xhigh" => ThinkingLevel::Xhigh,
        s if s == "max" => ThinkingLevel::Max,
        _ => return Err("unknown thinking level".to_string()),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Turn,
    Compaction,
    BranchSummary,
    Retry,
}

pub fn parse_session_phase(v: &JsonValue) -> VResult<SessionPhase> {
    Ok(match require_string(v, "phase", 1)? {
        s if s == "idle" => SessionPhase::Idle,
        s if s == "turn" => SessionPhase::Turn,
        s if s == "compaction" => SessionPhase::Compaction,
        s if s == "branch_summary" => SessionPhase::BranchSummary,
        s if s == "retry" => SessionPhase::Retry,
        _ => return Err("unknown session phase".to_string()),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

impl ModelRef {
    pub fn parse(v: &JsonValue) -> VResult<Self> {
        let map = require_object(v)?;
        strict_object(map, &["provider", "id"])?;
        Ok(Self {
            provider: require_string(get(map, "provider")?, "provider", 1)?,
            id: require_string(get(map, "id")?, "id", 1)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
}

impl ModelCost {
    pub fn parse(v: &JsonValue) -> VResult<Self> {
        let map = require_object(v)?;
        strict_object(map, &["input", "output", "cacheRead", "cacheWrite"])?;
        let number = |key: &str| -> VResult<f64> {
            match get(map, key)? {
                JsonValue::Number(n) => n
                    .as_f64()
                    .filter(|f| *f >= 0.0)
                    .ok_or_else(|| format!("{key} must be a number >= 0")),
                _ => Err(format!("{key} must be a number")),
            }
        };
        Ok(Self {
            input: number("input")?,
            output: number("output")?,
            cache_read: number("cacheRead")?,
            cache_write: number("cacheWrite")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub reasoning: bool,
    pub input: Vec<ModelInput>,
    #[serde(rename = "contextWindow")]
    pub context_window: i64,
    #[serde(rename = "maxTokens")]
    pub max_tokens: i64,
    pub cost: ModelCost,
    #[serde(rename = "supportedThinkingLevels")]
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    pub authenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    Text,
    Image,
}

impl ModelMetadata {
    pub fn parse(v: &JsonValue) -> VResult<Self> {
        let map = require_object(v)?;
        strict_object(
            map,
            &[
                "provider",
                "id",
                "name",
                "api",
                "reasoning",
                "input",
                "contextWindow",
                "maxTokens",
                "cost",
                "supportedThinkingLevels",
                "authenticated",
            ],
        )?;
        let input = match get(map, "input")? {
            JsonValue::Array(items) => {
                let mut parsed = Vec::with_capacity(items.len());
                for item in items {
                    match require_string(item, "input", 1)?.as_str() {
                        "text" => parsed.push(ModelInput::Text),
                        "image" => parsed.push(ModelInput::Image),
                        _ => return Err("unknown model input kind".to_string()),
                    }
                }
                parsed
            }
            _ => return Err("input must be an array".to_string()),
        };
        let supported = match get(map, "supportedThinkingLevels")? {
            JsonValue::Array(items) => {
                if items.is_empty() {
                    return Err("supportedThinkingLevels must have minItems 1".to_string());
                }
                let mut parsed = Vec::with_capacity(items.len());
                for item in items {
                    parsed.push(parse_thinking_level(item)?);
                }
                parsed
            }
            _ => return Err("supportedThinkingLevels must be an array".to_string()),
        };
        let authenticated = match get(map, "authenticated")? {
            JsonValue::Bool(b) => *b,
            _ => return Err("authenticated must be a boolean".to_string()),
        };
        let reasoning = match get(map, "reasoning")? {
            JsonValue::Bool(b) => *b,
            _ => return Err("reasoning must be a boolean".to_string()),
        };
        Ok(Self {
            provider: require_string(get(map, "provider")?, "provider", 1)?,
            id: require_string(get(map, "id")?, "id", 1)?,
            name: require_string(get(map, "name")?, "name", 1)?,
            api: require_string(get(map, "api")?, "api", 1)?,
            reasoning,
            input,
            context_window: require_integer(get(map, "contextWindow")?, "contextWindow", 1)?,
            max_tokens: require_integer(get(map, "maxTokens")?, "maxTokens", 1)?,
            cost: ModelCost::parse(get(map, "cost")?)?,
            supported_thinking_levels: supported,
            authenticated,
        })
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TextContent {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ThinkingContent {
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImageContent {
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolCallContent {
    #[serde(rename = "toolCall")]
    ToolCall {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        input: JsonValue,
    },
}

pub fn parse_text_content(v: &JsonValue) -> VResult<TextContent> {
    let map = require_object(v)?;
    strict_object(map, &["type", "text"])?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "text" => Ok(TextContent::Text {
            text: require_string(get(map, "text")?, "text", 0)?,
        }),
        _ => Err("expected type text".to_string()),
    }
}

pub fn parse_thinking_content(v: &JsonValue) -> VResult<ThinkingContent> {
    let map = require_object(v)?;
    strict_object(map, &["type", "thinking", "redacted"])?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "thinking" => {
            let redacted = match map.get("redacted") {
                Some(JsonValue::Bool(b)) => Some(*b),
                Some(_) => return Err("redacted must be a boolean".to_string()),
                None => None,
            };
            Ok(ThinkingContent::Thinking {
                thinking: require_string(get(map, "thinking")?, "thinking", 0)?,
                redacted,
            })
        }
        _ => Err("expected type thinking".to_string()),
    }
}

pub fn parse_image_content(v: &JsonValue) -> VResult<ImageContent> {
    let map = require_object(v)?;
    strict_object(map, &["type", "data", "mimeType"])?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "image" => Ok(ImageContent::Image {
            data: require_string(get(map, "data")?, "data", 0)?,
            mime_type: require_string(get(map, "mimeType")?, "mimeType", 1)?,
        }),
        _ => Err("expected type image".to_string()),
    }
}

pub fn parse_tool_call_content(v: &JsonValue) -> VResult<ToolCallContent> {
    let map = require_object(v)?;
    strict_object(map, &["type", "toolCallId", "toolName", "input"])?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "toolCall" => Ok(ToolCallContent::ToolCall {
            tool_call_id: require_string(get(map, "toolCallId")?, "toolCallId", 1)?,
            tool_name: require_string(get(map, "toolName")?, "toolName", 1)?,
            input: get(map, "input")?.clone(),
        }),
        _ => Err("expected type toolCall".to_string()),
    }
}

/// User-content block union (text only in the MVP; image is rejected by
/// `strict_object` on `prompt` command payloads).
pub fn parse_user_content(v: &JsonValue) -> VResult<UserContent> {
    let map = require_object(v)?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "text" => parse_text_content(v).map(UserContent::Text),
        "image" => parse_image_content(v).map(UserContent::Image),
        _ => Err("unknown user content type".to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}

pub fn parse_assistant_content(v: &JsonValue) -> VResult<AssistantContent> {
    let map = require_object(v)?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "text" => parse_text_content(v).map(AssistantContent::Text),
        "thinking" => parse_thinking_content(v).map(AssistantContent::Thinking),
        "toolCall" => parse_tool_call_content(v).map(AssistantContent::ToolCall),
        _ => Err("unknown assistant content type".to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCallContent),
}

pub fn parse_tool_content(v: &JsonValue) -> VResult<ToolContent> {
    let map = require_object(v)?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "text" => parse_text_content(v).map(ToolContent::Text),
        "image" => parse_image_content(v).map(ToolContent::Image),
        _ => Err("unknown tool content type".to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolContent {
    Text(TextContent),
    Image(ImageContent),
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    #[serde(rename = "cacheRead")]
    pub cache_read: i64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<i64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
    pub cost: UsageCost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
}

impl Usage {
    pub fn parse(v: &JsonValue) -> VResult<Self> {
        let map = require_object(v)?;
        strict_object(
            map,
            &[
                "input",
                "output",
                "cacheRead",
                "cacheWrite",
                "reasoning",
                "totalTokens",
                "cost",
            ],
        )?;
        let cost_map = require_object(get(map, "cost")?)?;
        strict_object(
            cost_map,
            &["input", "output", "cacheRead", "cacheWrite", "total"],
        )?;
        let cost_number = |key: &str| -> VResult<f64> {
            match cost_map.get(key) {
                Some(JsonValue::Number(n)) => n
                    .as_f64()
                    .filter(|f| *f >= 0.0)
                    .ok_or_else(|| format!("cost.{key} must be a number >= 0")),
                _ => Err(format!("cost.{key} must be a number")),
            }
        };
        let reasoning = match map.get("reasoning") {
            Some(JsonValue::Number(n)) => Some(
                n.as_i64()
                    .filter(|i| *i >= 0)
                    .ok_or_else(|| "reasoning must be an integer >= 0".to_string())?,
            ),
            Some(_) => return Err("reasoning must be an integer".to_string()),
            None => None,
        };
        Ok(Self {
            input: require_integer(get(map, "input")?, "input", 0)?,
            output: require_integer(get(map, "output")?, "output", 0)?,
            cache_read: require_integer(get(map, "cacheRead")?, "cacheRead", 0)?,
            cache_write: require_integer(get(map, "cacheWrite")?, "cacheWrite", 0)?,
            reasoning,
            total_tokens: require_integer(get(map, "totalTokens")?, "totalTokens", 0)?,
            cost: UsageCost {
                input: cost_number("input")?,
                output: cost_number("output")?,
                cache_read: cost_number("cacheRead")?,
                cache_write: cost_number("cacheWrite")?,
                total: cost_number("total")?,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Transcript items
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTranscriptItem {
    pub id: String,
    pub role: String, // "user"
    pub content: Vec<UserContent>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantStatus {
    Streaming,
    Complete,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTranscriptItem {
    pub id: String,
    pub role: String, // "assistant"
    pub content: Vec<AssistantContent>,
    pub model: ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "responseModel")]
    pub response_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub timestamp: i64,
    pub status: AssistantStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "stopReason")]
    pub stop_reason: Option<AssistantStopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "errorMessage")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantStopReason {
    Stop,
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Complete,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolTranscriptItem {
    pub id: String,
    pub role: String, // "tool"
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub input: JsonValue,
    pub content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub timestamp: i64,
    pub status: ToolStatus,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptItem {
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
    User(UserTranscriptItem),
}

// ---------------------------------------------------------------------------
// Progress, snapshots, errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptProgress {
    ItemStarted {
        item: TranscriptItem,
    },
    AssistantDelta {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "contentIndex")]
        content_index: i64,
        kind: TranscriptDeltaKind,
        delta: String,
    },
    ItemUpdated {
        item: TranscriptItemUpdate,
    },
    ItemFinished {
        item: TranscriptItemFinished,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptDeltaKind {
    Text,
    Thinking,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptItemUpdate {
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptItemFinished {
    CompleteAssistant(AssistantTranscriptItem),
    ErrorAssistant(AssistantTranscriptItem),
    AbortedAssistant(AssistantTranscriptItem),
    CompleteTool(ToolTranscriptItem),
    ErrorTool(ToolTranscriptItem),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "sessionName")]
    pub session_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    pub phase: SessionPhase,
    pub model: ModelRef,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: ThinkingLevel,
    pub attached: bool,
    pub locked: bool,
    pub revision: i64,
    pub transcript: Vec<TranscriptItem>,
    #[serde(rename = "queuedSteer")]
    pub queued_steer: Vec<UserTranscriptItem>,
    #[serde(rename = "queuedSteerCount")]
    pub queued_steer_count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerSnapshot {
    #[serde(rename = "serverId")]
    pub server_id: String,
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u64,
    pub revision: i64,
    pub sessions: Vec<SessionMetadata>,
    pub models: Vec<ModelMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    Version,
    Busy,
    SessionLocked,
    NotFound,
    InvalidRequest,
    NotImplemented,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
}

// ---------------------------------------------------------------------------
// Commands and results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    List,
    Create {
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "thinkingLevel")]
        thinking_level: Option<ThinkingLevel>,
    },
    Attach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Prompt {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    Steer {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    Abort {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    SetModel {
        #[serde(rename = "sessionId")]
        session_id: String,
        model: ModelRef,
    },
    SetThinking {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkingLevel,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandResult {
    List {
        sessions: Vec<SessionMetadata>,
    },
    Create {
        session: SessionSnapshot,
    },
    Attach {
        session: SessionSnapshot,
    },
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Prompt {
        session: SessionSnapshot,
    },
    Steer {
        session: SessionSnapshot,
    },
    Abort {
        session: SessionSnapshot,
    },
    SetModel {
        session: SessionSnapshot,
    },
    SetThinking {
        session: SessionSnapshot,
    },
}

// ---------------------------------------------------------------------------
// Client and server messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Hello { version: u64 },
    Request { id: String, request: Command },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerEvent {
    ServerSnapshot {
        snapshot: ServerSnapshot,
    },
    SessionSnapshot {
        snapshot: SessionSnapshot,
    },
    SessionProgress {
        #[serde(rename = "sessionId")]
        session_id: String,
        progress: TranscriptProgress,
    },
    SessionRemoved {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Hello {
        version: u64,
        #[serde(rename = "connectionId")]
        connection_id: String,
        snapshot: ServerSnapshot,
    },
    HelloError {
        error: ProtocolError,
    },
    #[serde(rename = "response")]
    Response {
        id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<CommandResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ProtocolError>,
    },
    #[serde(rename = "event")]
    Event {
        event: ServerEvent,
    },
}

pub fn is_supported_protocol_version(version: u64) -> bool {
    version == PROTOCOL_VERSION
}

// ---------------------------------------------------------------------------
// Wire validation
// ---------------------------------------------------------------------------

fn require_bool(value: &JsonValue, field: &str) -> VResult<bool> {
    match value {
        JsonValue::Bool(value) => Ok(*value),
        _ => Err(format!("{field} must be a boolean")),
    }
}

fn require_exact_string(value: &JsonValue, field: &str, expected: &str) -> VResult<()> {
    let actual = require_string(value, field, 0)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{field} must be {expected:?}"))
    }
}

fn require_u64(value: &JsonValue, field: &str, minimum: u64) -> VResult<u64> {
    match value {
        JsonValue::Number(number) => {
            if let Some(value) = number.as_u64() {
                if value >= minimum {
                    Ok(value)
                } else {
                    Err(format!("{field} must be >= {minimum}"))
                }
            } else {
                Err(format!("{field} must be an integer"))
            }
        }
        _ => Err(format!("{field} must be an integer")),
    }
}

fn optional_string(
    map: &serde_json::Map<String, JsonValue>,
    field: &str,
    min_length: usize,
) -> VResult<()> {
    if let Some(value) = map.get(field) {
        require_string(value, field, min_length)?;
    }
    Ok(())
}

fn validate_user_transcript_item(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    strict_object(map, &["id", "role", "content", "timestamp"])?;
    require_string(get(map, "id")?, "id", 1)?;
    require_exact_string(get(map, "role")?, "role", "user")?;
    let content = match get(map, "content")? {
        JsonValue::Array(content) => content,
        _ => return Err("content must be an array".to_string()),
    };
    for item in content {
        validate_user_content(item)?;
    }
    require_integer(get(map, "timestamp")?, "timestamp", 0)?;
    Ok(())
}

fn validate_user_content(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "text" => {
            parse_text_content(value)?;
            Ok(())
        }
        "image" => {
            parse_image_content(value)?;
            Ok(())
        }
        _ => Err("unknown user content type".to_string()),
    }
}

fn validate_assistant_transcript_item(value: &JsonValue) -> VResult<String> {
    let map = require_object(value)?;
    let status = require_string(get(map, "status")?, "status", 1)?;
    let allowed = match status.as_str() {
        "streaming" => &[
            "id",
            "role",
            "content",
            "model",
            "responseModel",
            "usage",
            "timestamp",
            "status",
        ][..],
        "complete" => &[
            "id",
            "role",
            "content",
            "model",
            "responseModel",
            "usage",
            "timestamp",
            "status",
            "stopReason",
        ][..],
        "error" | "aborted" => &[
            "id",
            "role",
            "content",
            "model",
            "responseModel",
            "usage",
            "timestamp",
            "status",
            "stopReason",
            "errorMessage",
        ][..],
        _ => return Err("unknown assistant status".to_string()),
    };
    strict_object(map, allowed)?;
    require_string(get(map, "id")?, "id", 1)?;
    require_exact_string(get(map, "role")?, "role", "assistant")?;
    let content = match get(map, "content")? {
        JsonValue::Array(content) => content,
        _ => return Err("content must be an array".to_string()),
    };
    for item in content {
        validate_assistant_content(item)?;
    }
    ModelRef::parse(get(map, "model")?)?;
    optional_string(map, "responseModel", 1)?;
    if let Some(usage) = map.get("usage") {
        Usage::parse(usage)?;
    }
    require_integer(get(map, "timestamp")?, "timestamp", 0)?;

    match status.as_str() {
        "streaming" => {}
        "complete" => match require_string(get(map, "stopReason")?, "stopReason", 1)?.as_str() {
            "stop" | "length" | "toolUse" => {}
            _ => return Err("invalid complete assistant stopReason".to_string()),
        },
        "error" => {
            require_exact_string(get(map, "stopReason")?, "stopReason", "error")?;
            optional_string(map, "errorMessage", 1)?;
        }
        "aborted" => {
            require_exact_string(get(map, "stopReason")?, "stopReason", "aborted")?;
            optional_string(map, "errorMessage", 0)?;
        }
        _ => unreachable!("assistant status was checked above"),
    }
    Ok(status)
}

fn validate_assistant_content(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    match require_string(get(map, "type")?, "type", 1)?.as_str() {
        "text" => {
            parse_text_content(value)?;
            Ok(())
        }
        "thinking" => {
            parse_thinking_content(value)?;
            Ok(())
        }
        "toolCall" => {
            parse_tool_call_content(value)?;
            Ok(())
        }
        _ => Err("unknown assistant content type".to_string()),
    }
}

fn validate_tool_transcript_item(value: &JsonValue) -> VResult<String> {
    let map = require_object(value)?;
    strict_object(
        map,
        &[
            "id",
            "role",
            "toolCallId",
            "toolName",
            "input",
            "content",
            "details",
            "usage",
            "timestamp",
            "status",
            "isError",
        ],
    )?;
    require_string(get(map, "id")?, "id", 1)?;
    require_exact_string(get(map, "role")?, "role", "tool")?;
    require_string(get(map, "toolCallId")?, "toolCallId", 1)?;
    require_string(get(map, "toolName")?, "toolName", 1)?;
    let content = match get(map, "content")? {
        JsonValue::Array(content) => content,
        _ => return Err("content must be an array".to_string()),
    };
    for item in content {
        let item_map = require_object(item)?;
        match require_string(get(item_map, "type")?, "type", 1)?.as_str() {
            "text" | "image" => {
                parse_tool_content(item)?;
            }
            _ => return Err("unknown tool content type".to_string()),
        }
    }
    if let Some(usage) = map.get("usage") {
        Usage::parse(usage)?;
    }
    require_integer(get(map, "timestamp")?, "timestamp", 0)?;
    let status = require_string(get(map, "status")?, "status", 1)?;
    let is_error = require_bool(get(map, "isError")?, "isError")?;
    match status.as_str() {
        "running" | "complete" if !is_error => {}
        "error" if is_error => {}
        "running" | "complete" => {
            return Err("running and complete tool items must have isError false".to_string())
        }
        "error" => return Err("error tool items must have isError true".to_string()),
        _ => return Err("unknown tool status".to_string()),
    }
    Ok(status)
}

fn validate_transcript_item(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    match require_string(get(map, "role")?, "role", 1)?.as_str() {
        "user" => validate_user_transcript_item(value),
        "assistant" => validate_assistant_transcript_item(value).map(|_| ()),
        "tool" => validate_tool_transcript_item(value).map(|_| ()),
        _ => Err("unknown transcript role".to_string()),
    }
}

fn validate_transcript_progress(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    let kind = require_string(get(map, "type")?, "type", 1)?;
    match kind.as_str() {
        "item_started" => {
            strict_object(map, &["type", "item"])?;
            validate_transcript_item(get(map, "item")?)?;
        }
        "assistant_delta" => {
            strict_object(map, &["type", "messageId", "contentIndex", "kind", "delta"])?;
            require_string(get(map, "messageId")?, "messageId", 1)?;
            require_integer(get(map, "contentIndex")?, "contentIndex", 0)?;
            match require_string(get(map, "kind")?, "kind", 1)?.as_str() {
                "text" | "thinking" | "toolCall" => {}
                _ => return Err("unknown assistant delta kind".to_string()),
            }
            require_string(get(map, "delta")?, "delta", 0)?;
        }
        "item_updated" => {
            strict_object(map, &["type", "item"])?;
            let item = get(map, "item")?;
            let item_map = require_object(item)?;
            match require_string(get(item_map, "role")?, "role", 1)?.as_str() {
                "assistant" => {
                    validate_assistant_transcript_item(item)?;
                }
                "tool" => {
                    validate_tool_transcript_item(item)?;
                }
                _ => return Err("item_updated only accepts assistant or tool items".to_string()),
            }
        }
        "item_finished" => {
            strict_object(map, &["type", "item"])?;
            let item = get(map, "item")?;
            let item_map = require_object(item)?;
            let role = require_string(get(item_map, "role")?, "role", 1)?;
            match role.as_str() {
                "assistant" => match validate_assistant_transcript_item(item)?.as_str() {
                    "complete" | "error" | "aborted" => {}
                    _ => return Err("item_finished requires a finished assistant item".to_string()),
                },
                "tool" => match validate_tool_transcript_item(item)?.as_str() {
                    "complete" | "error" => {}
                    _ => return Err("item_finished requires a finished tool item".to_string()),
                },
                _ => return Err("item_finished only accepts assistant or tool items".to_string()),
            }
        }
        _ => return Err("unknown transcript progress type".to_string()),
    }
    Ok(())
}

fn validate_session_metadata(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    strict_object(
        map,
        &[
            "id",
            "createdAt",
            "updatedAt",
            "parentSessionId",
            "sessionName",
            "cwd",
        ],
    )?;
    require_string(get(map, "id")?, "id", 1)?;
    require_integer(get(map, "createdAt")?, "createdAt", 0)?;
    if let Some(value) = map.get("updatedAt") {
        require_integer(value, "updatedAt", 0)?;
    }
    optional_string(map, "parentSessionId", 1)?;
    optional_string(map, "sessionName", 0)?;
    optional_string(map, "cwd", 1)?;
    Ok(())
}

fn validate_session_snapshot(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    strict_object(
        map,
        &[
            "id",
            "name",
            "cwd",
            "createdAt",
            "updatedAt",
            "phase",
            "model",
            "thinkingLevel",
            "attached",
            "locked",
            "revision",
            "transcript",
            "queuedSteer",
            "queuedSteerCount",
        ],
    )?;
    require_string(get(map, "id")?, "id", 1)?;
    optional_string(map, "name", 0)?;
    require_string(get(map, "cwd")?, "cwd", 1)?;
    require_integer(get(map, "createdAt")?, "createdAt", 0)?;
    require_integer(get(map, "updatedAt")?, "updatedAt", 0)?;
    parse_session_phase(get(map, "phase")?)?;
    ModelRef::parse(get(map, "model")?)?;
    parse_thinking_level(get(map, "thinkingLevel")?)?;
    require_bool(get(map, "attached")?, "attached")?;
    require_bool(get(map, "locked")?, "locked")?;
    require_integer(get(map, "revision")?, "revision", 0)?;
    let transcript = match get(map, "transcript")? {
        JsonValue::Array(transcript) => transcript,
        _ => return Err("transcript must be an array".to_string()),
    };
    for item in transcript {
        validate_transcript_item(item)?;
    }
    let queued_steer = match get(map, "queuedSteer")? {
        JsonValue::Array(items) => items,
        _ => return Err("queuedSteer must be an array".to_string()),
    };
    for item in queued_steer {
        validate_user_transcript_item(item)?;
    }
    require_integer(get(map, "queuedSteerCount")?, "queuedSteerCount", 0)?;
    Ok(())
}

fn validate_server_snapshot(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    strict_object(
        map,
        &[
            "serverId",
            "protocolVersion",
            "revision",
            "sessions",
            "models",
        ],
    )?;
    require_string(get(map, "serverId")?, "serverId", 1)?;
    require_u64(
        get(map, "protocolVersion")?,
        "protocolVersion",
        PROTOCOL_VERSION,
    )?;
    if get(map, "protocolVersion")? != &JsonValue::from(PROTOCOL_VERSION) {
        return Err("protocolVersion must be 1".to_string());
    }
    require_integer(get(map, "revision")?, "revision", 0)?;
    let sessions = match get(map, "sessions")? {
        JsonValue::Array(sessions) => sessions,
        _ => return Err("sessions must be an array".to_string()),
    };
    for session in sessions {
        validate_session_metadata(session)?;
    }
    let models = match get(map, "models")? {
        JsonValue::Array(models) => models,
        _ => return Err("models must be an array".to_string()),
    };
    for model in models {
        ModelMetadata::parse(model)?;
    }
    Ok(())
}

fn validate_protocol_error(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    strict_object(map, &["code", "message", "details"])?;
    match require_string(get(map, "code")?, "code", 1)?.as_str() {
        "version" | "busy" | "session_locked" | "not_found" | "invalid_request"
        | "not_implemented" | "internal_error" => {}
        _ => return Err("unknown protocol error code".to_string()),
    }
    require_string(get(map, "message")?, "message", 0)?;
    Ok(())
}

fn validate_command(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    let command = require_string(get(map, "command")?, "command", 1)?;
    match command.as_str() {
        "list" => strict_object(map, &["command"]),
        "create" => {
            strict_object(map, &["command", "cwd", "name", "model", "thinkingLevel"])?;
            optional_string(map, "cwd", 1)?;
            optional_string(map, "name", 0)?;
            if let Some(model) = map.get("model") {
                ModelRef::parse(model)?;
            }
            if let Some(thinking) = map.get("thinkingLevel") {
                parse_thinking_level(thinking)?;
            }
            Ok(())
        }
        "attach" | "detach" | "abort" => {
            strict_object(map, &["command", "sessionId"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
            Ok(())
        }
        "prompt" | "steer" => {
            strict_object(map, &["command", "sessionId", "text"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
            require_string(get(map, "text")?, "text", 0)?;
            Ok(())
        }
        "set_model" => {
            strict_object(map, &["command", "sessionId", "model"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
            ModelRef::parse(get(map, "model")?)?;
            Ok(())
        }
        "set_thinking" => {
            strict_object(map, &["command", "sessionId", "thinkingLevel"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
            parse_thinking_level(get(map, "thinkingLevel")?)?;
            Ok(())
        }
        _ => Err("unknown command".to_string()),
    }
}

fn validate_command_result(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    let command = require_string(get(map, "command")?, "command", 1)?;
    match command.as_str() {
        "list" => {
            strict_object(map, &["command", "sessions"])?;
            let sessions = match get(map, "sessions")? {
                JsonValue::Array(sessions) => sessions,
                _ => return Err("sessions must be an array".to_string()),
            };
            for session in sessions {
                validate_session_metadata(session)?;
            }
            Ok(())
        }
        "detach" => {
            strict_object(map, &["command", "sessionId"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
            Ok(())
        }
        "create" | "attach" | "prompt" | "steer" | "abort" | "set_model" | "set_thinking" => {
            strict_object(map, &["command", "session"])?;
            validate_session_snapshot(get(map, "session")?)
        }
        _ => Err("unknown command result".to_string()),
    }
}

fn validate_server_event(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    let event = require_string(get(map, "type")?, "type", 1)?;
    match event.as_str() {
        "server_snapshot" => {
            strict_object(map, &["type", "snapshot"])?;
            validate_server_snapshot(get(map, "snapshot")?)?;
        }
        "session_snapshot" => {
            strict_object(map, &["type", "snapshot"])?;
            validate_session_snapshot(get(map, "snapshot")?)?;
        }
        "session_progress" => {
            strict_object(map, &["type", "sessionId", "progress"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
            validate_transcript_progress(get(map, "progress")?)?;
        }
        "session_removed" => {
            strict_object(map, &["type", "sessionId"])?;
            require_string(get(map, "sessionId")?, "sessionId", 1)?;
        }
        _ => return Err("unknown server event".to_string()),
    }
    Ok(())
}

/// Validates a JSON representation against the pinned upstream client schema.
pub fn validate_client_message(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    let message_type = require_string(get(map, "type")?, "type", 1)?;
    match message_type.as_str() {
        "hello" => {
            strict_object(map, &["type", "version"])?;
            require_integer(get(map, "version")?, "version", 0)?;
        }
        "request" => {
            strict_object(map, &["type", "id", "request"])?;
            require_string(get(map, "id")?, "id", 1)?;
            validate_command(get(map, "request")?)?;
        }
        _ => return Err("unknown client message type".to_string()),
    }
    Ok(())
}

/// Validates a JSON representation against the pinned upstream server schema.
pub fn validate_server_message(value: &JsonValue) -> VResult<()> {
    let map = require_object(value)?;
    let message_type = require_string(get(map, "type")?, "type", 1)?;
    match message_type.as_str() {
        "hello" => {
            strict_object(map, &["type", "version", "connectionId", "snapshot"])?;
            if require_u64(get(map, "version")?, "version", PROTOCOL_VERSION)? != PROTOCOL_VERSION {
                return Err("version must be 1".to_string());
            }
            require_string(get(map, "connectionId")?, "connectionId", 1)?;
            validate_server_snapshot(get(map, "snapshot")?)?;
        }
        "hello_error" => {
            strict_object(map, &["type", "error"])?;
            validate_protocol_error(get(map, "error")?)?;
        }
        "response" => {
            let ok = require_bool(get(map, "ok")?, "ok")?;
            if ok {
                strict_object(map, &["type", "id", "ok", "result"])?;
                require_string(get(map, "id")?, "id", 1)?;
                validate_command_result(get(map, "result")?)?;
            } else {
                strict_object(map, &["type", "id", "ok", "error"])?;
                require_string(get(map, "id")?, "id", 1)?;
                validate_protocol_error(get(map, "error")?)?;
            }
        }
        "event" => {
            strict_object(map, &["type", "event"])?;
            validate_server_event(get(map, "event")?)?;
        }
        _ => return Err("unknown server message type".to_string()),
    }
    Ok(())
}
