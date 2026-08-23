//! RPC protocol types — port of
//! `packages/coding-agent/src/modes/rpc/rpc-types.ts`.
//!
//! Commands arrive as JSON objects on stdin (discriminated by `type`), with
//! an optional `id` for correlation. Responses are `{"id", "type":
//! "response", "command", "success", data|error}` on stdout. Events are
//! streamed as `message_update`/`agent_settled` records.

/// A parsed `rpc_command` value. Field access is provided by helpers so
/// unknown/absent fields behave like upstream (optional reads).
#[derive(Debug, Clone)]
pub struct RpcCommand {
    pub id: Option<String>,
    pub type_: String,
    pub value: serde_json::Value,
}

impl RpcCommand {
    pub fn parse(value: serde_json::Value) -> Result<Self, String> {
        let type_ = value
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "command missing type".to_string())?
            .to_string();
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(Self { id, type_, value })
    }

    pub fn str_field(&self, name: &str) -> Option<String> {
        self.value
            .get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
    pub fn bool_field(&self, name: &str) -> Option<bool> {
        self.value.get(name).and_then(|v| v.as_bool())
    }
    pub fn image_field(&self) -> Option<Vec<serde_json::Value>> {
        self.value.get("images").and_then(|v| v.as_array()).cloned()
    }
}

/// Build a success response object (upstream `success` helper).
pub fn success(
    id: Option<&str>,
    command: &str,
    data: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    if let Some(id) = id {
        obj.insert("id".to_string(), serde_json::Value::String(id.to_string()));
    }
    obj.insert(
        "type".to_string(),
        serde_json::Value::String("response".to_string()),
    );
    obj.insert(
        "command".to_string(),
        serde_json::Value::String(command.to_string()),
    );
    obj.insert("success".to_string(), serde_json::Value::Bool(true));
    if let Some(data) = data {
        obj.insert("data".to_string(), data);
    }
    serde_json::Value::Object(obj)
}

/// Build an error response object (upstream `error` helper).
pub fn failure(id: Option<&str>, command: &str, message: String) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    if let Some(id) = id {
        obj.insert("id".to_string(), serde_json::Value::String(id.to_string()));
    }
    obj.insert(
        "type".to_string(),
        serde_json::Value::String("response".to_string()),
    );
    obj.insert(
        "command".to_string(),
        serde_json::Value::String(command.to_string()),
    );
    obj.insert("success".to_string(), serde_json::Value::Bool(false));
    obj.insert("error".to_string(), serde_json::Value::String(message));
    serde_json::Value::Object(obj)
}

/// RPC session state (upstream `RpcSessionState`) — rendered as JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
    pub model: Option<serde_json::Value>,
    pub thinking_level: String,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
    pub session_file: Option<String>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
}

/// Compaction result shape (upstream `CompactionResult`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RpcCompactionResult {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    #[serde(
        rename = "estimatedTokensAfter",
        skip_serializing_if = "Option::is_none"
    )]
    pub estimated_tokens_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_id_and_type() {
        let cmd =
            RpcCommand::parse(serde_json::json!({"id": "1", "type": "prompt", "message": "hi"}))
                .unwrap();
        assert_eq!(cmd.type_, "prompt");
        assert_eq!(cmd.id.as_deref(), Some("1"));
        assert_eq!(cmd.str_field("message").as_deref(), Some("hi"));
    }

    #[test]
    fn parse_requires_type() {
        assert!(RpcCommand::parse(serde_json::json!({"id": "1"})).is_err());
    }

    #[test]
    fn success_response_shape() {
        let resp = success(Some("7"), "get_state", None);
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["command"], "get_state");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["id"], "7");
        assert!(resp.get("data").is_none());
    }

    #[test]
    fn failure_response_shape() {
        let resp = failure(Some("7"), "prompt", "boom".to_string());
        assert_eq!(resp["success"], false);
        assert_eq!(resp["error"], "boom");
    }
}
