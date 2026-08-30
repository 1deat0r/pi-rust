//! Usage accounting for subprocess-backed evals.
//!
//! The pinned upstream eval harness obtains these values from
//! `AgentSession.getSessionStats()`. A subprocess cannot access that object,
//! so the equivalent source of truth is the session JSONL written by `pi`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::EvalError;

/// Usage totals reconstructed from one or more session JSONL entries.
///
/// Token fields are signed because the session format permits adjustment
/// records. This matches the signed `Usage` representation used by the Rust
/// session writer while preserving the upstream sum semantics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsage {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub tool_calls: u64,
    pub estimated_cost_usd: Option<f64>,
}

impl SessionUsage {
    /// Adds totals from another session snapshot, retaining the latest model
    /// identity seen in the input stream.
    pub fn merge(&mut self, other: &Self) {
        self.provider = other.provider.clone().or_else(|| self.provider.clone());
        self.model = other.model.clone().or_else(|| self.model.clone());
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.total_tokens += other.total_tokens;
        self.tool_calls += other.tool_calls;
        self.estimated_cost_usd = match (self.estimated_cost_usd, other.estimated_cost_usd) {
            (Some(left), Some(right)) => Some(left + right),
            (Some(cost), None) | (None, Some(cost)) => Some(cost),
            (None, None) => None,
        };
    }

    fn add_usage(&mut self, usage: &Value) -> Result<(), EvalError> {
        let object = usage.as_object().ok_or(EvalError::UsageNotObject)?;
        self.input_tokens += signed_integer(object, "input")?;
        self.output_tokens += signed_integer(object, "output")?;
        self.cache_read_tokens += signed_integer(object, "cacheRead")?;
        self.cache_write_tokens += signed_integer(object, "cacheWrite")?;
        self.total_tokens = self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens;

        if let Some(cost) = cost_total(object)? {
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        }
        Ok(())
    }
}

/// The JSONL body and accounting derived from one subprocess session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSnapshot {
    pub path: PathBuf,
    pub jsonl: String,
    pub usage: SessionUsage,
}

/// Parses either the current v4 mutation JSONL or the pinned upstream v3
/// session-entry JSONL and reconstructs the same billed totals as
/// `getSessionStats()`/`addUsageToTotals()`.
pub fn parse_session_usage(session_jsonl: &str) -> Result<SessionUsage, EvalError> {
    let mut usage = SessionUsage::default();
    let mut saw_header = false;

    for (index, raw_line) in session_jsonl.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(line).map_err(|source| EvalError::ParseSessionLine {
                line: line_number,
                source,
            })?;
        let object = value
            .as_object()
            .ok_or(EvalError::SessionLineNotObject { line: line_number })?;

        if !saw_header {
            if object.get("kind").and_then(Value::as_str) == Some("header")
                && object.get("version").and_then(Value::as_u64) == Some(4)
            {
                saw_header = true;
                continue;
            }
            if object.get("type").and_then(Value::as_str) == Some("session") {
                saw_header = true;
                continue;
            }
            return Err(EvalError::UnsupportedSessionHeader { line: line_number });
        }

        // v4 lane/fact/record mutations are not session messages and are not
        // included by the upstream getSessionStats() loop. The v4 entry is
        // the mutation itself; legacy files put the same entry shape at the
        // top level without a `kind` discriminator.
        let is_v4_entry = object.get("kind").and_then(Value::as_str) == Some("entry");
        let is_legacy_entry = object.get("kind").is_none();
        if !is_v4_entry && !is_legacy_entry {
            continue;
        }

        let Some(entry_type) = object.get("type").and_then(Value::as_str) else {
            continue;
        };
        match entry_type {
            "message" => {
                let Some(message) = object.get("message").and_then(Value::as_object) else {
                    return Err(EvalError::SessionMessageNotObject { line: line_number });
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        if let Some(provider) = message.get("provider").and_then(Value::as_str) {
                            usage.provider = Some(provider.to_string());
                        }
                        let model = message
                            .get("responseModel")
                            .and_then(Value::as_str)
                            .or_else(|| message.get("model").and_then(Value::as_str));
                        if let Some(model) = model {
                            usage.model = Some(model.to_string());
                        }
                        if let Some(content) = message.get("content").and_then(Value::as_array) {
                            usage.tool_calls += content
                                .iter()
                                .filter(|part| {
                                    part.get("type").and_then(Value::as_str) == Some("toolCall")
                                })
                                .count() as u64;
                        }
                        if let Some(message_usage) = message.get("usage") {
                            if !message_usage.is_null() {
                                usage.add_usage(message_usage).map_err(|source| {
                                    EvalError::SessionLineUsage {
                                        line: line_number,
                                        source: Box::new(source),
                                    }
                                })?;
                            }
                        }
                    }
                    Some("toolResult") => {
                        if let Some(message_usage) = message.get("usage") {
                            if !message_usage.is_null() {
                                usage.add_usage(message_usage).map_err(|source| {
                                    EvalError::SessionLineUsage {
                                        line: line_number,
                                        source: Box::new(source),
                                    }
                                })?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            "compaction" | "branch_summary" => {
                if let Some(entry_usage) = object.get("usage") {
                    if !entry_usage.is_null() {
                        usage.add_usage(entry_usage).map_err(|source| {
                            EvalError::SessionLineUsage {
                                line: line_number,
                                source: Box::new(source),
                            }
                        })?;
                    }
                }
            }
            _ => {}
        }
    }

    if !saw_header {
        return Err(EvalError::MissingSessionHeader);
    }
    Ok(usage)
}

/// Finds and parses the newest JSONL session below `session_root`.
pub fn read_latest_session_snapshot(
    session_root: &Path,
) -> Result<Option<SessionSnapshot>, EvalError> {
    if !session_root.exists() {
        return Ok(None);
    }
    let mut files = Vec::new();
    collect_jsonl_files(session_root, &mut files)?;
    files.sort_by(|left, right| {
        let left_modified = std::fs::metadata(left)
            .and_then(|metadata| metadata.modified())
            .ok();
        let right_modified = std::fs::metadata(right)
            .and_then(|metadata| metadata.modified())
            .ok();
        left_modified
            .cmp(&right_modified)
            .then_with(|| left.cmp(right))
    });
    let Some(path) = files.pop() else {
        return Ok(None);
    };
    let jsonl = std::fs::read_to_string(&path).map_err(|source| EvalError::ReadSession {
        path: path.clone(),
        source,
    })?;
    let usage = parse_session_usage(&jsonl)?;
    Ok(Some(SessionSnapshot { path, jsonl, usage }))
}

fn collect_jsonl_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), EvalError> {
    for entry in std::fs::read_dir(directory).map_err(|source| EvalError::ListSessionDir {
        path: directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| EvalError::InspectSessionDir {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| EvalError::InspectSessionPath {
                path: path.clone(),
                source,
            })?;
        if file_type.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn signed_integer(object: &serde_json::Map<String, Value>, field: &str) -> Result<i64, EvalError> {
    let Some(value) = object.get(field) else {
        return Ok(0);
    };
    if let Some(value) = value.as_i64() {
        return Ok(value);
    }
    if let Some(value) = value.as_u64() {
        return i64::try_from(value).map_err(|_| EvalError::UsageFieldOutOfRange {
            field: field.to_string(),
        });
    }
    Err(EvalError::UsageFieldNotInteger {
        field: field.to_string(),
    })
}

fn cost_total(usage: &serde_json::Map<String, Value>) -> Result<Option<f64>, EvalError> {
    let Some(cost) = usage.get("cost") else {
        return Ok(None);
    };
    if cost.is_null() {
        return Ok(None);
    }
    let cost = cost.as_object().ok_or(EvalError::CostNotObject)?;
    let components = [
        ("input", None),
        ("output", None),
        ("cacheRead", Some("cache_read")),
        ("cacheWrite", Some("cache_write")),
    ];
    let component_total = components.iter().try_fold(0.0, |total, field| {
        Ok::<_, EvalError>(total + finite_number_alias(cost, field.0, field.1)?)
    })?;
    let total = if cost.contains_key("total") {
        finite_number_alias(cost, "total", None)?
    } else {
        component_total
    };
    if total == 0.0 && component_total == 0.0 {
        Ok(None)
    } else {
        Ok(Some(total))
    }
}

fn finite_number_alias(
    object: &serde_json::Map<String, Value>,
    field: &str,
    alias: Option<&str>,
) -> Result<f64, EvalError> {
    if let Some(value) = object.get(field) {
        return finite_value(value, field);
    }
    if let Some(alias) = alias {
        if let Some(value) = object.get(alias) {
            return finite_value(value, alias);
        }
    }
    Ok(0.0)
}

fn finite_value(value: &Value, field: &str) -> Result<f64, EvalError> {
    let number = value
        .as_f64()
        .ok_or_else(|| EvalError::CostFieldNotNumber {
            field: field.to_string(),
        })?;
    if !number.is_finite() {
        return Err(EvalError::CostFieldNotFinite {
            field: field.to_string(),
        });
    }
    Ok(number)
}
