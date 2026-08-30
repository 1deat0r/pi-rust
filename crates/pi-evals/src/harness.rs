//! Eval harness surface — port of `packages/evals/src/pi-harness.ts` and the
//! `vitest-evals/harness` contract subset the ported machinery needs.
//!
//! The upstream harness adapts an in-process `AgentSession` to
//! `vitest-evals`. The Rust port runs the real `pi` binary as a subprocess
//! and maps its transcript to the same harness result shape (output, events,
//! usage, timings, artifacts).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::session_usage::{read_latest_session_snapshot, SessionUsage};

/// JSON value used throughout harness results.
pub type JsonValue = serde_json::Value;

/// Model selection (provider + id) — port of `PiCodingAgentModelSelection`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: String,
    pub id: String,
}

/// Resolves an explicit harness model or the `PI_PROVIDER`/`PI_MODEL`
/// environment defaults (port of `resolveModelSelection`).
pub fn resolve_model_selection(
    explicit_model: Option<&ModelSelection>,
    environment: Option<(&str, &str)>,
) -> Result<ModelSelection, EvalError> {
    // `explicitModel?.provider ?? environment.PI_PROVIDER` — an explicit
    // (even empty) value shadows the environment; the result must be a
    // non-empty trimmed pair.
    let provider = explicit_model
        .as_ref()
        .map(|m| m.provider.trim().to_string())
        .or_else(|| environment.map(|(p, _)| p.trim().to_string()))
        .filter(|s| !s.is_empty());
    let id = explicit_model
        .as_ref()
        .map(|m| m.id.trim().to_string())
        .or_else(|| environment.map(|(_, m)| m.trim().to_string()))
        .filter(|s| !s.is_empty());
    match (provider, id) {
        (Some(provider), Some(id)) => Ok(ModelSelection { provider, id }),
        _ => Err(EvalError::MissingModelSelection),
    }
}

/// Transcript event (subset of `vitest-evals` TranscriptEvent used by the
/// flattened session trace).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptEvent {
    Message {
        role: String,
        content: String,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<JsonValue>,
    },
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        name: String,
        content: JsonValue,
        #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
        error: serde_json::Map<String, JsonValue>,
    },
}

/// Harness usage (port of the upstream `usage` block).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUsage {
    pub provider: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub tool_calls: u64,
    pub metadata: BTreeMap<String, JsonValue>,
}

impl HarnessUsage {
    /// Converts session-file totals into the upstream harness usage shape.
    pub fn from_session_usage(
        usage: &SessionUsage,
        fallback_provider: &str,
        fallback_model: &str,
    ) -> Self {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "cacheReadTokens".to_string(),
            serde_json::json!(usage.cache_read_tokens),
        );
        metadata.insert(
            "cacheWriteTokens".to_string(),
            serde_json::json!(usage.cache_write_tokens),
        );
        if let Some(cost) = usage.estimated_cost_usd {
            metadata.insert("estimatedCostUsd".to_string(), serde_json::json!(cost));
        }
        Self {
            provider: usage
                .provider
                .clone()
                .unwrap_or_else(|| fallback_provider.to_string()),
            model: usage
                .model
                .clone()
                .unwrap_or_else(|| fallback_model.to_string()),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            tool_calls: usage.tool_calls,
            metadata,
        }
    }
}

/// Harness result (subset of `SimpleHarnessResult<TOutput>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessResult<TOutput> {
    pub output: TOutput,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    pub events: Vec<TranscriptEvent>,
    pub usage: HarnessUsage,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<HarnessTimings>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HarnessTimings {
    pub total_ms: f64,
}

/// Context captured per harness run (port of `HarnessContext`).
#[derive(Debug, Clone, Default)]
pub struct HarnessContext {
    pub artifacts: BTreeMap<String, JsonValue>,
    pub artifact_directory: Option<std::path::PathBuf>,
}

impl HarnessContext {
    pub fn set_artifact(&mut self, name: &str, value: JsonValue) {
        self.artifacts.insert(name.to_string(), value);
    }
}

/// Harness run-closure signature.
type HarnessRunFn<TOutput> = std::sync::Arc<
    dyn Fn(&serde_json::Value, &mut HarnessContext) -> HarnessResult<TOutput> + Send + Sync,
>;

/// A single eval harness (port of the `Harness` interface).
#[derive(Clone)]
pub struct Harness<TOutput = String> {
    pub name: String,
    pub run: HarnessRunFn<TOutput>,
}

impl<TOutput> Harness<TOutput> {
    pub fn new(
        name: impl Into<String>,
        run: impl Fn(&serde_json::Value, &mut HarnessContext) -> HarnessResult<TOutput>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            run: std::sync::Arc::new(run),
        }
    }

    pub fn run(
        &self,
        input: &serde_json::Value,
        context: &mut HarnessContext,
    ) -> HarnessResult<TOutput> {
        (self.run)(input, context)
    }
}

/// Options for running the real `pi` binary (port of the harness options).
#[derive(Debug, Clone)]
pub struct PiCliRunnerOptions {
    pub binary: String,
    pub provider: String,
    pub model: String,
    pub no_tools: bool,
    /// Extra args appended before the prompt.
    pub extra_args: Vec<String>,
    /// Maximum wall time for one prompt (seconds). Default 180.
    pub timeout_secs: u64,
}

impl Default for PiCliRunnerOptions {
    fn default() -> Self {
        Self {
            binary: "pi".to_string(),
            provider: "faux".to_string(),
            model: "faux-1".to_string(),
            no_tools: true,
            extra_args: Vec::new(),
            timeout_secs: 180,
        }
    }
}

/// Result of one `pi` subprocess invocation.
#[derive(Debug, Clone)]
pub struct PiRunOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub session_jsonl: Option<String>,
    pub usage: SessionUsage,
}

/// Runs the real `pi` binary once with a prompt in the given working
/// directory (`pi -p --provider X --model Y [--no-tools] [extra] <prompt>`).
/// Each public call gets an isolated session, matching the standalone harness
/// contract. Multi-step evals use [`run_pi_binary_in_session`] below so a
/// reload/continuation is a real persisted-session operation rather than a
/// second unrelated subprocess.
pub fn run_pi_binary(
    options: &PiCliRunnerOptions,
    cwd: &Path,
    prompt: &str,
) -> Result<PiRunOutput, EvalError> {
    let session_root = create_session_root(cwd)?;
    let result = run_pi_binary_in_session(options, cwd, prompt, &session_root, false);
    // The public one-shot harness owns this isolated session directory. The
    // durable JSONL has already been copied into the returned value, so do
    // not leave a temp session tree behind after the run settles.
    let cleanup = std::fs::remove_dir_all(&session_root);
    if let Err(source) = cleanup {
        if result.is_ok() {
            return Err(EvalError::CleanupSessionDir {
                path: session_root.clone(),
                source,
            });
        }
    }
    result
}

/// Run one prompt against an explicitly retained session directory.
/// `continue_session` maps to the CLI's real `-c/--continue` selector, so the
/// child process restores the latest durable session before handling the new
/// prompt.
pub(crate) fn run_pi_binary_in_session(
    options: &PiCliRunnerOptions,
    cwd: &Path,
    prompt: &str,
    session_root: &Path,
    continue_session: bool,
) -> Result<PiRunOutput, EvalError> {
    std::fs::create_dir_all(session_root)
        .map_err(|source| EvalError::CreateSessionDir { source })?;
    let agent_dir = cwd.join(".pi").join("agent");
    std::fs::create_dir_all(&agent_dir).map_err(|source| EvalError::CreateAgentDir { source })?;
    let mut command = std::process::Command::new(&options.binary);
    command.current_dir(cwd);
    command.arg("-p");
    if continue_session {
        command.arg("--continue");
    }
    command.args(["--provider", &options.provider]);
    command.args(["--model", &options.model]);
    if options.no_tools {
        command.arg("--no-tools");
    }
    for arg in &options.extra_args {
        command.arg(arg);
    }
    command.arg(prompt);
    command.env("PI_EVAL_TIMEOUT", "1");
    command.env("PI_CODING_AGENT_DIR", &agent_dir);
    command.env("PI_CODING_AGENT_SESSION_DIR", session_root);
    command.env("PI_SKIP_VERSION_CHECK", "1");
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command.spawn().map_err(|source| EvalError::Spawn {
        binary: options.binary.clone(),
        source,
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(options.timeout_secs);
    let mut killed = false;
    let output = loop {
        // Poll completion without blocking past the deadline.
        let Some(_status) = child
            .try_wait()
            .map_err(|source| EvalError::Wait { source })?
        else {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                killed = true;
                break child
                    .wait_with_output()
                    .map_err(|source| EvalError::Wait { source })?;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        };
        break child
            .wait_with_output()
            .map_err(|source| EvalError::Wait { source })?;
    };
    if killed {
        return Err(EvalError::Timeout {
            binary: options.binary.clone(),
            timeout_secs: options.timeout_secs,
        });
    }
    let exit_code = output.status.code().unwrap_or(-1);
    let snapshot = read_latest_session_snapshot(session_root)?;
    let (session_jsonl, usage) = match snapshot {
        Some(snapshot) => (Some(snapshot.jsonl), snapshot.usage),
        None => (None, SessionUsage::default()),
    };
    Ok(PiRunOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code,
        session_jsonl,
        usage,
    })
}

pub(crate) fn create_session_root(cwd: &Path) -> Result<PathBuf, EvalError> {
    let session_root = cwd
        .join(".pi")
        .join("eval-sessions")
        .join(crate::harness_table::short_id());
    std::fs::create_dir_all(&session_root)
        .map_err(|source| EvalError::CreateSessionDir { source })?;
    Ok(session_root)
}

/// Convert the durable session message entries into the eval transcript
/// events exposed by the upstream in-process harness. This intentionally
/// reads the persisted message shape rather than inventing a two-event
/// prompt/answer trace, so tool calls and tool results remain observable.
pub fn transcript_events_from_session_jsonl(
    session_jsonl: &str,
) -> Result<Vec<TranscriptEvent>, EvalError> {
    let mut events = Vec::new();
    for (index, raw_line) in session_jsonl.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let value: JsonValue =
            serde_json::from_str(line).map_err(|source| EvalError::ParseTranscriptLine {
                line: line_number,
                source,
            })?;
        let object = value
            .as_object()
            .ok_or(EvalError::TranscriptLineNotObject { line: line_number })?;
        let entry_type = object.get("type").and_then(JsonValue::as_str);
        let is_message = entry_type == Some("message")
            && (object.get("kind").is_none()
                || object.get("kind").and_then(JsonValue::as_str) == Some("entry"));
        if !is_message {
            continue;
        }
        let Some(message) = object.get("message").and_then(JsonValue::as_object) else {
            continue;
        };
        let Some(role) = message.get("role").and_then(JsonValue::as_str) else {
            continue;
        };
        let content = message.get("content").cloned().unwrap_or(JsonValue::Null);
        match role {
            "user" => {
                events.push(TranscriptEvent::Message {
                    role: "user".to_string(),
                    content: content_text(&content),
                });
            }
            "assistant" => {
                let text = content_text(&content);
                if !text.is_empty() {
                    events.push(TranscriptEvent::Message {
                        role: "assistant".to_string(),
                        content: text,
                    });
                }
                if let Some(parts) = content.as_array() {
                    for part in parts {
                        if part.get("type").and_then(JsonValue::as_str) != Some("toolCall") {
                            continue;
                        }
                        let Some(id) = part.get("id").and_then(JsonValue::as_str) else {
                            continue;
                        };
                        let Some(name) = part.get("name").and_then(JsonValue::as_str) else {
                            continue;
                        };
                        events.push(TranscriptEvent::ToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments: part.get("arguments").and_then(|arguments| {
                                arguments.as_object().map(|_| arguments.clone())
                            }),
                        });
                    }
                }
            }
            "toolResult" => {
                let Some(tool_call_id) = message.get("toolCallId").and_then(JsonValue::as_str)
                else {
                    continue;
                };
                let name = message
                    .get("toolName")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                let is_error = message
                    .get("isError")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let text = content_text(&content);
                let mut error = serde_json::Map::new();
                if is_error {
                    error.insert(
                        "message".to_string(),
                        JsonValue::String(if text.is_empty() {
                            "Tool failed".to_string()
                        } else {
                            text.clone()
                        }),
                    );
                }
                events.push(TranscriptEvent::ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    name: name.to_string(),
                    content: text_or_json_content(&content),
                    error,
                });
            }
            _ => {}
        }
    }
    Ok(events)
}

/// Return the durable session id from a session JSONL header when present.
pub fn session_id_from_session_jsonl(session_jsonl: &str) -> Option<String> {
    session_jsonl.lines().find_map(|raw_line| {
        let value: JsonValue = serde_json::from_str(raw_line.trim()).ok()?;
        let object = value.as_object()?;
        let is_header = object.get("kind").and_then(JsonValue::as_str) == Some("header")
            || object.get("type").and_then(JsonValue::as_str) == Some("session");
        is_header
            .then(|| {
                object
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            })
            .flatten()
    })
}

fn content_text(content: &JsonValue) -> String {
    match content {
        JsonValue::String(text) => text.clone(),
        JsonValue::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(JsonValue::as_str) == Some("text"))
            .map(|part| {
                part.get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn text_or_json_content(content: &JsonValue) -> JsonValue {
    if content.as_array().is_some_and(|parts| {
        parts
            .iter()
            .all(|part| part.get("type").and_then(JsonValue::as_str) == Some("text"))
    }) {
        JsonValue::String(content_text(content))
    } else {
        content.clone()
    }
}

/// Extracts the answer text from a `pi -p` stdout transcript (the print path
/// writes the final assistant text on stdout; diagnostics go to stderr).
pub fn extract_response_text(line: &str) -> String {
    line.trim().to_string()
}

/// Creates a temporary eval root (mirror of `mkdtemp(join(tmpdir(), "pi-eval-"))`).
pub fn create_eval_root() -> std::io::Result<std::path::PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "pi-eval-{}-{}",
        std::process::id(),
        crate::harness_table::short_id()
    ));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

impl<TOutput> std::fmt::Debug for Harness<TOutput> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness").field("name", &self.name).finish()
    }
}
