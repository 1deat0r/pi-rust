//! Bash tool — port of `packages/agent/src/harness/tools/bash.ts`, including
//! bounded live output updates and a final progress snapshot.

use super::truncate::{format_size, DEFAULT_MAX_BYTES};
use super::{AgentToolResult, ToolUpdateCallback};
use crate::harness::env::{ExecutionErrorCode, StdExecutionEnv};
use crate::harness::shell_output::{
    execute_shell_with_capture, ChunkHandlerWithProgress, ShellCaptureOptions,
    ShellCaptureProgress, ShellCaptureResult, TruncationResult,
};
use crate::types::FileError;
use pi_ai::types::ToolResultMessage;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;
const BASH_UPDATE_THROTTLE_MS: u64 = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct BashCapture {
    pub output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub truncation_message: String,
    pub full_output_path: Option<String>,
    pub timed_out: bool,
    pub aborted: bool,
    pub error_message: Option<String>,
}

/// Validates a bash timeout (seconds). Mirrors upstream `validateTimeout`.
pub fn validate_timeout(timeout: Option<f64>) -> Result<(), String> {
    match timeout {
        None => Ok(()),
        Some(v) => {
            if !v.is_finite() || v <= 0.0 {
                Err("Invalid timeout: must be a finite number of seconds".to_string())
            } else if v > MAX_TIMEOUT_SECONDS {
                Err(format!(
                    "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Runs a bash command in `cwd` with inherit env, capturing combined output.
pub async fn run_bash(
    command: &str,
    cwd: &str,
    timeout_secs: Option<f64>,
    abort: Option<Arc<AtomicBool>>,
) -> Result<BashCapture, FileError> {
    validate_timeout(timeout_secs).map_err(FileError::new)?;

    // Keep every public bash entry point on the same bounded capture path as
    // the upstream harness so RPC/non-update calls retain the full-output
    // artifact when the displayed tail is truncated.
    let capture = run_bash_with_output(command, cwd, timeout_secs, abort, None).await?;
    if !capture.aborted && !capture.timed_out {
        if let Some(error) = capture.error_message.clone() {
            return Err(FileError::new(error));
        }
    }
    Ok(capture)
}

/// Receives the current combined stdout/stderr snapshot while a direct bash
/// command is running. The callback is invoked from the async shell-capture
/// path, never from a detached renderer thread.
pub type BashOutputCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Run a direct interactive bash command through the shared shell-capture
/// implementation. This variant preserves the streamed output callback and
/// the final truncation/full-output metadata needed by the interactive TUI's
/// `BashExecution` record.
pub async fn run_bash_with_output(
    command: &str,
    cwd: &str,
    timeout_secs: Option<f64>,
    abort: Option<Arc<AtomicBool>>,
    on_output: Option<BashOutputCallback>,
) -> Result<BashCapture, FileError> {
    validate_timeout(timeout_secs).map_err(FileError::new)?;

    let on_chunk: Option<ChunkHandlerWithProgress> = on_output.map(|callback| {
        Arc::new(
            move |_chunk: &str, progress: &Mutex<ShellCaptureProgress>| {
                callback(
                    progress
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .output
                        .clone(),
                );
                Ok(())
            },
        ) as ChunkHandlerWithProgress
    });
    let environment = StdExecutionEnv::new(cwd.to_string());
    let capture = execute_shell_with_capture(
        &environment,
        command,
        &ShellCaptureOptions {
            cwd: Some(cwd.to_string()),
            env: None,
            timeout: timeout_secs,
            abort,
            on_chunk,
            inherit_env: true,
            return_execution_errors: true,
        },
    )
    .await
    .map_err(|error| FileError::new(error.to_string()))?;

    let timed_out = capture
        .execution_error
        .as_ref()
        .is_some_and(|error| error.code == ExecutionErrorCode::Timeout);
    let error_message = capture
        .execution_error
        .as_ref()
        .map(|error| error.message.clone());
    let formatted = format_bash_output(&capture);
    let truncation_message = formatted
        .strip_prefix(&capture.output)
        .unwrap_or_default()
        .to_string();
    Ok(BashCapture {
        output: capture.output,
        exit_code: capture.exit_code,
        truncated: capture.truncated,
        truncation_message,
        full_output_path: capture.full_output_path,
        timed_out,
        aborted: capture.cancelled,
        error_message,
    })
}

/// Execute handler for the agent tool: validates, runs, and renders the
/// tool-result message with upstream status semantics.
pub async fn execute_bash(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    execute_bash_with_abort(command, timeout, cwd, None).await
}

/// Execute bash with the agent-loop cancellation flag attached.
pub async fn execute_bash_with_abort(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
    abort: Option<Arc<AtomicBool>>,
) -> Result<ToolResultMessage, String> {
    validate_timeout(timeout)?;
    let capture = run_bash(command, cwd, timeout, abort)
        .await
        .map_err(|e| e.to_string())?;

    let output_text = format!("{}{}", capture.output, capture.truncation_message);
    let append_status = |status: String| -> String {
        if output_text.is_empty() {
            status
        } else {
            format!("{output_text}\n\n{status}")
        }
    };

    if capture.aborted {
        return Err(append_status("Operation aborted".to_string()));
    }
    if capture.timed_out {
        return Err(append_status(format!(
            "Command timed out after {} seconds",
            timeout.unwrap_or(0.0)
        )));
    }
    if capture.exit_code == Some(0) {
        Ok(ToolResultMessage::text(
            "bash",
            "bash",
            if output_text.is_empty() {
                "(no output)".to_string()
            } else {
                output_text
            },
            false,
        ))
    } else {
        Err(append_status(format!(
            "Command exited with code {}",
            capture.exit_code.unwrap_or(0)
        )))
    }
}

/// Execute bash while forwarding partial combined output through the
/// AgentTool `onUpdate` contract.
pub async fn execute_bash_with_updates(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
    abort: Option<Arc<AtomicBool>>,
    on_update: Option<ToolUpdateCallback>,
) -> Result<AgentToolResult, String> {
    validate_timeout(timeout)?;

    if let Some(on_update) = &on_update {
        on_update(&AgentToolResult::default());
    }

    let last_update_at = Arc::new(Mutex::new(
        Instant::now() - Duration::from_millis(BASH_UPDATE_THROTTLE_MS),
    ));
    let output_callback: Option<ChunkHandlerWithProgress> = on_update.as_ref().map(|on_update| {
        let on_update = on_update.clone();
        let last_update_at = last_update_at.clone();
        Arc::new(
            move |_chunk: &str, progress: &Mutex<ShellCaptureProgress>| {
                let should_emit = {
                    let mut last_update_at = last_update_at
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if last_update_at.elapsed() < Duration::from_millis(BASH_UPDATE_THROTTLE_MS) {
                        false
                    } else {
                        *last_update_at = Instant::now();
                        true
                    }
                };
                if should_emit {
                    let progress = progress
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    emit_bash_partial(
                        &on_update,
                        progress.output.clone(),
                        bash_progress_details(&progress),
                    );
                }
                Ok(())
            },
        ) as ChunkHandlerWithProgress
    });
    let env = StdExecutionEnv::new(cwd.to_string());
    let capture = execute_shell_with_capture(
        &env,
        command,
        &ShellCaptureOptions {
            cwd: Some(cwd.to_string()),
            timeout,
            abort,
            on_chunk: output_callback,
            inherit_env: true,
            return_execution_errors: true,
            ..Default::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    if let Some(on_update) = &on_update {
        let progress = ShellCaptureProgress {
            output: capture.output.clone(),
            truncation: capture.truncation.clone(),
            full_output_path: capture.full_output_path.clone(),
            last_line_bytes: capture.last_line_bytes,
        };
        emit_bash_partial(
            on_update,
            progress.output.clone(),
            bash_progress_details(&progress),
        );
    }

    let output_text = format_bash_output(&capture);
    let append_status = |status: String| -> String {
        if output_text.is_empty() {
            status
        } else {
            format!("{output_text}\n\n{status}")
        }
    };

    if capture.cancelled {
        return Err(append_status("Command aborted".to_string()));
    }
    if capture
        .execution_error
        .as_ref()
        .is_some_and(|error| error.code == ExecutionErrorCode::Timeout)
    {
        return Err(append_status(format!(
            "Command timed out after {} seconds",
            timeout.unwrap_or(0.0)
        )));
    }
    if let Some(error) = &capture.execution_error {
        return Err(append_status(error.message.clone()));
    }
    if capture.exit_code != Some(0) {
        Err(append_status(format!(
            "Command exited with code {}",
            capture.exit_code.unwrap_or(0)
        )))
    } else {
        Ok(AgentToolResult {
            content: vec![pi_ai::types::ContentBlock::text(
                if output_text.is_empty() {
                    "(no output)".to_string()
                } else {
                    output_text
                },
            )],
            details: if capture.truncated {
                Some(bash_progress_details(&ShellCaptureProgress {
                    output: capture.output.clone(),
                    truncation: capture.truncation.clone(),
                    full_output_path: capture.full_output_path.clone(),
                    last_line_bytes: capture.last_line_bytes,
                }))
            } else {
                None
            },
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        })
    }
}

fn format_bash_output(capture: &ShellCaptureResult) -> String {
    if !capture.truncation.truncated {
        return capture.output.clone();
    }

    let start_line = capture
        .truncation
        .total_lines
        .saturating_sub(capture.truncation.output_lines)
        + 1;
    let end_line = capture.truncation.total_lines;
    let full_output_path = capture
        .full_output_path
        .as_deref()
        .unwrap_or("<full output unavailable>");
    let message = if capture.truncation.last_line_partial {
        format!(
            "\n\n[Showing last {} of line {end_line} (line is {}). Full output: {full_output_path}]",
            format_size(capture.truncation.output_bytes),
            format_size(capture.last_line_bytes),
        )
    } else if capture.truncation.truncated_by == Some(crate::tools::truncate::TruncatedBy::Lines) {
        format!(
            "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {full_output_path}]",
            capture.truncation.total_lines,
        )
    } else {
        format!(
            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {full_output_path}]",
            capture.truncation.total_lines,
            format_size(DEFAULT_MAX_BYTES),
        )
    };
    format!("{}{}", capture.output, message)
}

fn bash_progress_details(progress: &ShellCaptureProgress) -> serde_json::Value {
    let mut details = serde_json::Map::new();
    if progress.truncation.truncated {
        details.insert(
            "truncation".to_string(),
            truncation_to_json(&progress.truncation),
        );
    }
    if let Some(path) = &progress.full_output_path {
        details.insert("fullOutputPath".to_string(), serde_json::json!(path));
    }
    serde_json::Value::Object(details)
}

fn truncation_to_json(truncation: &TruncationResult) -> serde_json::Value {
    serde_json::json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": truncation.truncated_by.map(|kind| match kind {
            crate::tools::truncate::TruncatedBy::Lines => "lines",
            crate::tools::truncate::TruncatedBy::Bytes => "bytes",
        }),
        "totalLines": truncation.total_lines,
        "totalBytes": truncation.total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": truncation.last_line_partial,
        "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
        "maxLines": truncation.max_lines,
        "maxBytes": truncation.max_bytes,
    })
}

fn emit_bash_partial(on_update: &ToolUpdateCallback, output: String, details: serde_json::Value) {
    on_update(&AgentToolResult {
        content: vec![pi_ai::types::ContentBlock::text(output)],
        details: Some(details),
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
    });
}
