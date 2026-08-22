//! JSONL v4 session file codec — port of
//! `packages/agent/src/harness/session/jsonl/codec.ts`.

pub mod errors;
pub mod repo;
pub mod storage;

use serde_json::Value as JsonValue;

pub use errors::{InvalidSessionFileError, JsonlDecodeError, JsonlDecodeErrorKind};

use super::types::{
    Entry, Fact, JsonlV4Header, LaneRecord, Mutation, SessionMetadata,
};

// Port of ENTRY_TYPES / RECORD_TYPES / OPERATION_KINDS.
pub const ENTRY_TYPES: [&str; 7] = [
    "message",
    "model_change",
    "thinking_level_change",
    "active_tools_change",
    "compaction",
    "branch_summary",
    "custom",
];
pub const RECORD_TYPES: [&str; 9] = [
    "operation_started",
    "abort_requested",
    "operation_finished",
    "step_attempt",
    "tool_started",
    "queue_enqueued",
    "queue_cancelled",
    "write_deferred",
    "usage",
];
pub const OPERATION_KINDS: [&str; 3] = ["run", "compaction", "navigation"];

fn is_object(value: &JsonValue) -> bool {
    matches!(value, JsonValue::Object(_))
}

/// Parses a JSONL line into an object, mapping JSON syntax errors to
/// `JsonlDecodeError::Syntax`.
fn parse_object(line: &str) -> Result<serde_json::Map<String, JsonValue>, JsonlDecodeError> {
    let value: JsonValue = serde_json::from_str(line)
        .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Syntax, "is not valid JSON"))?;
    match value {
        JsonValue::Object(map) => Ok(map),
        _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "is not a JSON object")),
    }
}

fn require_string(map: &serde_json::Map<String, JsonValue>, field: &str) -> Result<String, JsonlDecodeError> {
    require_string_label(map, field, field)
}

fn require_string_label(
    map: &serde_json::Map<String, JsonValue>,
    field: &str,
    label: &str,
) -> Result<String, JsonlDecodeError> {
    match map.get(field) {
        Some(JsonValue::String(s)) => Ok(s.clone()),
        _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, format!("has invalid {label}"))),
    }
}

fn require_sequence(value: Option<&JsonValue>) -> Result<u64, JsonlDecodeError> {
    match value {
        Some(JsonValue::Number(n)) => n.as_u64().filter(|v| *v > 0).ok_or_else(|| {
            JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid seq")
        }),
        _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid seq")),
    }
}

fn require_timestamp(value: Option<&JsonValue>) -> Result<u64, JsonlDecodeError> {
    match value {
        Some(JsonValue::Number(n)) => n.as_u64().ok_or_else(|| {
            JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid timestamp")
        }),
        _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid timestamp")),
    }
}

fn require_nullable_id(value: Option<&JsonValue>, field: &str) -> Result<Option<String>, JsonlDecodeError> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, format!("has invalid {field}"))),
    }
}

/// Validated header decode. Mirrors `decodeHeader`.
fn decode_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    let value = parse_object(line)?;
    if !matches!(value.get("kind"), Some(JsonValue::String(k)) if k == "header") {
        return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "is not a header"));
    }
    let version = match value.get("version").and_then(|v| v.as_u64()) {
        Some(4) => 4,
        _ => return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has unsupported session version")),
    };
    let parent_session_id = match value.get("parentSessionId") {
        Some(JsonValue::String(s)) => Some(s.clone()),
        None => None,
        Some(_) => return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid parentSessionId")),
    };
    let legacy_parent_session_path = match value.get("legacyParentSessionPath") {
        Some(JsonValue::String(s)) => Some(s.clone()),
        None => None,
        Some(_) => {
            return Err(JsonlDecodeError::new(
                JsonlDecodeErrorKind::Schema,
                "has invalid legacyParentSessionPath",
            ))
        }
    };
    if parent_session_id.is_some() && legacy_parent_session_path.is_some() {
        return Err(JsonlDecodeError::new(
            JsonlDecodeErrorKind::Schema,
            "has both parentSessionId and legacyParentSessionPath",
        ));
    }
    let metadata = match value.get("metadata") {
        None => None,
        Some(JsonValue::Object(_)) => Some(value.get("metadata").cloned().unwrap()),
        Some(_) => return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid metadata")),
    };
    Ok(JsonlV4Header {
        kind: "header".into(),
        version,
        id: require_string(&value, "id")?,
        created_at: require_timestamp(value.get("createdAt"))?,
        cwd: require_string(&value, "cwd")?,
        parent_session_id,
        legacy_parent_session_path,
        metadata,
    })
}

pub fn parse_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    decode_header(line)
}

pub fn encode_header(header: &JsonlV4Header) -> Result<String, serde_json::Error> {
    Ok(format!("{}\n", serde_json::to_string(header)?))
}

pub fn metadata_from_header(
    header: &JsonlV4Header,
    path: &str,
    modified_at: u64,
) -> SessionMetadata {
    SessionMetadata {
        id: header.id.clone(),
        created_at: header.created_at,
        cwd: header.cwd.clone(),
        path: path.to_string(),
        modified_at,
        source_format: 4,
        parent_session_id: header.parent_session_id.clone(),
        legacy_parent_session_path: header.legacy_parent_session_path.clone(),
        metadata: header.metadata.clone(),
    }
}

/// Decodes one mutation line with strict field validation (mirrors
/// `decodeMutation` + `parseEntryMutation`/`parseRecordMutation`/...).
fn decode_mutation(line: &str) -> Result<Mutation, JsonlDecodeError> {
    let value = parse_object(line)?;
    let seq = require_sequence(value.get("seq"))?;
    match value.get("kind").and_then(|v| v.as_str()) {
        Some("entry") => {
            let lane = require_nullable_id(value.get("lane"), "lane")?;
            let id = require_string(&value, "id")?;
            let entry_type = require_string_label(&value, "type", "entry type")?;
            if !ENTRY_TYPES.contains(&entry_type.as_str()) {
                return Err(JsonlDecodeError::new(
                    JsonlDecodeErrorKind::Schema,
                    format!("has unknown entry type {entry_type}"),
                ));
            }
            let parent_id = require_nullable_id(value.get("parentId"), "parentId")?;
            let timestamp = require_timestamp(value.get("timestamp"))?;
            if entry_type == "custom" {
                require_string(&value, "customType")?;
            }
            let entry = build_entry_from_map(&value, entry_type, id, parent_id, seq, timestamp)?;
            Ok(Mutation::Entry { lane, entry })
        }
        Some("record") => {
            let id = require_string(&value, "id")?;
            let lane = require_string(&value, "lane")?;
            let record_type = require_string_label(&value, "type", "record type")?;
            if !RECORD_TYPES.contains(&record_type.as_str()) {
                return Err(JsonlDecodeError::new(
                    JsonlDecodeErrorKind::Schema,
                    format!("has unknown record type {record_type}"),
                ));
            }
            let timestamp = require_timestamp(value.get("timestamp"))?;
            if record_type == "operation_started" {
                let intent = value.get("intent").ok_or_else(|| {
                    JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid intent")
                })?;
                if !is_object(intent) {
                    return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid intent"));
                }
                let operation_kind = require_string_label(intent.as_object().unwrap(), "kind", "operation kind")?;
                if !OPERATION_KINDS.contains(&operation_kind.as_str()) {
                    return Err(JsonlDecodeError::new(
                        JsonlDecodeErrorKind::Schema,
                        format!("has unknown operation kind {operation_kind}"),
                    ));
                }
            }
            if record_type == "operation_finished" {
                require_string(&value, "runId")?;
            }
            let record = build_record_from_map(&value, record_type, id, lane, seq, timestamp)?;
            Ok(Mutation::Record { record })
        }
        Some("lane") => Ok(Mutation::Lane {
            seq,
            lane: require_string(&value, "lane")?,
            leaf_id: require_nullable_id(value.get("leafId"), "leafId")?,
        }),
        Some("fact") => {
            let fact = require_string_label(&value, "fact", "fact type")?;
            match fact.as_str() {
                "name" => {
                    if let Some(name) = value.get("name") {
                        if !name.is_string() {
                            return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid name"));
                        }
                    }
                    let name = value.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
                    Ok(Mutation::Fact(Fact::Name { seq, name }))
                }
                "label" => {
                    let target_id = require_string(&value, "targetId")?;
                    let label = match value.get("label") {
                        None => None,
                        Some(JsonValue::String(s)) => Some(s.clone()),
                        Some(_) => {
                            return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid label"))
                        }
                    };
                    Ok(Mutation::Fact(Fact::Label { seq, target_id, label }))
                }
                _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has unknown fact type")),
            }
        }
        _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has unknown mutation kind")),
    }
}

pub fn parse_mutation(line: &str) -> Result<Mutation, JsonlDecodeError> {
    decode_mutation(line)
}

/// Serializes one mutation. Entry/record variants spread the full object
/// (including storage-assigned fields) under `kind`, matching upstream
/// `encodeMutation`.
pub fn encode_mutation(mutation: &Mutation) -> Result<String, serde_json::Error> {
    // Need exact upstream ordering semantics — serialize with the `kind`
    // first, then lane/fields. serde_json Map preserves insertion order for
    // Object values but our tagged enum serialization handles placement.
    let json = serde_json::to_value(mutation)?;
    Ok(format!("{}\n", serde_json::to_string(&json)?))
}

// --- Entry/record build helpers: build typed values from the raw maps ---

fn build_entry_from_map(
    map: &serde_json::Map<String, JsonValue>,
    entry_type: String,
    id: String,
    parent_id: Option<String>,
    seq: u64,
    timestamp: u64,
) -> Result<Entry, JsonlDecodeError> {
    Ok(match entry_type.as_str() {
        "message" => Entry::Message {
            id,
            seq,
            parent_id,
            timestamp,
            message: serde_json::from_value(
                map.get("message").cloned().ok_or_else(|| {
                    JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid message")
                })?,
            )
            .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid message"))?,
            terminate: map.get("terminate").and_then(|v| v.as_bool()),
        },
        "model_change" => Entry::ModelChange {
            id, seq, parent_id, timestamp,
            provider: require_string(map, "provider")?,
            model_id: require_string(map, "modelId")?,
        },
        "thinking_level_change" => Entry::ThinkingLevel {
            id, seq, parent_id, timestamp,
            thinking_level: require_string(map, "thinkingLevel")?,
        },
        "active_tools_change" => Entry::ActiveTools {
            id, seq, parent_id, timestamp,
            active_tool_names: match map.get("activeToolNames") {
                Some(JsonValue::Array(items)) => items
                    .iter()
                    .map(|v| match v {
                        JsonValue::String(s) => Ok(s.clone()),
                        _ => Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid activeToolNames")),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid activeToolNames")),
            },
        },
        "compaction" => Entry::Compaction {
            id, seq, parent_id, timestamp,
            summary: require_string(map, "summary")?,
            retained_tail: match map.get("retainedTail") {
                Some(JsonValue::Array(items)) => items
                    .iter()
                    .map(|v| serde_json::from_value(v.clone())
                        .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid retainedTail")))
                    .collect::<Result<Vec<_>, _>>()?,
                _ => Vec::new(),
            },
            tokens_before: match map.get("tokensBefore").and_then(|v| v.as_u64()) {
                Some(v) => v,
                None => return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid tokensBefore")),
            },
            details: map.get("details").cloned(),
            usage: map.get("usage").cloned().map(|v| serde_json::from_value(v).unwrap_or_default()),
        },
        "branch_summary" => Entry::BranchSummary {
            id, seq, parent_id, timestamp,
            from_id: require_string(map, "fromId")?,
            summary: require_string(map, "summary")?,
            details: map.get("details").cloned(),
            usage: map.get("usage").cloned().map(|v| serde_json::from_value(v).unwrap_or_default()),
        },
        "custom" => Entry::Custom {
            id, seq, parent_id, timestamp,
            custom_type: require_string(map, "customType")?,
            data: map.get("data").cloned(),
        },
        _ => unreachable!("entry type validated above"),
    })
}

fn build_record_from_map(
    map: &serde_json::Map<String, JsonValue>,
    record_type: String,
    id: String,
    lane: String,
    seq: u64,
    timestamp: u64,
) -> Result<LaneRecord, JsonlDecodeError> {
    Ok(match record_type.as_str() {
        "operation_started" => LaneRecord::OperationStarted {
            id, seq, lane, timestamp,
            source_leaf_id: require_nullable_id(map.get("sourceLeafId"), "sourceLeafId")?,
            intent: serde_json::from_value(
                map.get("intent").cloned().ok_or_else(|| {
                    JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid intent")
                })?,
            )
            .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid intent"))?,
        },
        "abort_requested" => LaneRecord::AbortRequested {
            id, seq, lane, timestamp,
            run_id: require_string(map, "runId")?,
        },
        "operation_finished" => LaneRecord::OperationFinished {
            id, seq, lane, timestamp,
            run_id: require_string(map, "runId")?,
            outcome: require_string(map, "outcome")?,
            error: map.get("error").cloned().and_then(|v| serde_json::from_value(v).ok()),
        },
        "step_attempt" => LaneRecord::StepAttempt {
            id, seq, lane, timestamp,
            run_id: require_string(map, "runId")?,
            step: require_string(map, "step")?,
            attempt: match map.get("attempt").and_then(|v| v.as_u64()) {
                Some(v) => v,
                None => return Err(JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid attempt")),
            },
            result_entry_id: require_string(map, "resultEntryId")?,
            compaction_reason: map.get("compactionReason").and_then(|v| v.as_str()).map(|s| s.to_string()),
        },
        "tool_started" => LaneRecord::ToolStarted {
            id, seq, lane, timestamp,
            run_id: require_string(map, "runId")?,
            assistant_entry_id: require_string(map, "assistantEntryId")?,
            tool_index: map.get("toolIndex").and_then(|v| v.as_u64())
                .ok_or_else(|| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid toolIndex"))?,
            tool_call_id: require_string(map, "toolCallId")?,
            tool_name: require_string(map, "toolName")?,
            effective_args: map.get("effectiveArgs").cloned()
                .ok_or_else(|| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid effectiveArgs"))?,
            result_entry_id: require_string(map, "resultEntryId")?,
            replay: require_string(map, "replay")?,
        },
        "queue_enqueued" => LaneRecord::QueueEnqueued {
            id, seq, lane, timestamp,
            queue: require_string(map, "queue")?,
            run_id: require_string(map, "runId")?,
            target: map.get("target").cloned()
                .ok_or_else(|| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid target"))?,
        },
        "queue_cancelled" => LaneRecord::QueueCancelled {
            id, seq, lane, timestamp,
            run_id: map.get("runId").and_then(|v| v.as_str()).map(|s| s.to_string()),
            entry_id: require_string(map, "entryId")?,
        },
        "write_deferred" => LaneRecord::WriteDeferred {
            id, seq, lane, timestamp,
            run_id: require_string(map, "runId")?,
            target: map.get("target").cloned()
                .ok_or_else(|| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid target"))?,
        },
        "usage" => LaneRecord::Usage {
            id, seq, lane, timestamp,
            cause: require_string(map, "cause")?,
            run_id: require_string(map, "runId")?,
            entry_id: require_string(map, "entryId")?,
            attempt: map.get("attempt").and_then(|v| v.as_u64())
                .ok_or_else(|| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid attempt"))?,
            stop_reason: map.get("stopReason").and_then(|v| v.as_str()).map(|s| s.to_string()),
            tool_call_id: map.get("toolCallId").and_then(|v| v.as_str()).map(|s| s.to_string()),
            details: map.get("details").cloned(),
            usage: serde_json::from_value(
                map.get("usage").cloned().ok_or_else(|| {
                    JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid usage")
                })?,
            )
            .map_err(|_| JsonlDecodeError::new(JsonlDecodeErrorKind::Schema, "has invalid usage"))?,
        },
        _ => unreachable!("record type validated above"),
    })
}
