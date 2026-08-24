//! Eval harness surface — port of `packages/evals/src/pi-harness.ts` and the
//! `vitest-evals/harness` contract subset the ported machinery needs.
//!
//! The upstream harness adapts an in-process `AgentSession` to
//! `vitest-evals`. The Rust port runs the real `pi` binary as a subprocess
//! and maps its transcript to the same harness result shape (output, events,
//! usage, timings, artifacts).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
) -> Result<ModelSelection, String> {
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
        _ => Err(
            "Select a harness model explicitly or set both PI_PROVIDER and PI_MODEL as defaults."
                .to_string(),
        ),
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
pub fn run_pi_binary(
    options: &PiCliRunnerOptions,
    cwd: &std::path::Path,
    prompt: &str,
) -> Result<PiRunOutput, String> {
    let session_root = cwd
        .join(".pi")
        .join("eval-sessions")
        .join(crate::harness_table::short_id());
    std::fs::create_dir_all(&session_root)
        .map_err(|error| format!("failed to create eval session directory: {error}"))?;
    let agent_dir = cwd.join(".pi").join("agent");
    std::fs::create_dir_all(&agent_dir)
        .map_err(|error| format!("failed to create eval agent directory: {error}"))?;
    let mut command = std::process::Command::new(&options.binary);
    command.current_dir(cwd);
    command.arg("-p");
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
    command.env("PI_CODING_AGENT_SESSION_DIR", &session_root);
    command.env("PI_SKIP_VERSION_CHECK", "1");
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn {}: {error}", options.binary))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(options.timeout_secs);
    let mut killed = false;
    let output = loop {
        // Poll completion without blocking past the deadline.
        let Some(_status) = child
            .try_wait()
            .map_err(|error| format!("failed to wait: {error}"))?
        else {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                killed = true;
                break child
                    .wait_with_output()
                    .map_err(|error| format!("failed to wait: {error}"))?;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        };
        break child
            .wait_with_output()
            .map_err(|error| format!("failed to wait: {error}"))?;
    };
    if killed {
        return Err(format!(
            "{} timed out after {}s",
            options.binary, options.timeout_secs
        ));
    }
    let exit_code = output.status.code().unwrap_or(-1);
    let snapshot = read_latest_session_snapshot(&session_root)?;
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

/// Extracts the answer text from a `pi -p` stdout transcript (the print path
/// writes the final assistant text on stdout; diagnostics go to stderr).
pub fn extract_response_text(line: &str) -> String {
    line.trim().to_string()
}

/// Creates a temporary eval root (mirror of `mkdtemp(join(tmpdir(), "pi-eval-"))`).
pub fn create_eval_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pi-eval-{}-{}",
        std::process::id(),
        crate::harness_table::short_id()
    ));
    std::fs::create_dir_all(&root).expect("create eval root");
    root
}

impl<TOutput> std::fmt::Debug for Harness<TOutput> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness").field("name", &self.name).finish()
    }
}
