//! Bash tool — port of `packages/agent/src/harness/tools/bash.ts`, including
//! bounded live output updates and a final progress snapshot.

use super::truncate::{format_size, truncate_tail, DEFAULT_MAX_BYTES};
use super::{AgentToolResult, ToolUpdateCallback};
use crate::types::FileError;
use pi_ai::types::ToolResultMessage;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub timed_out: bool,
    pub aborted: bool,
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
    run_bash_with_callback(command, cwd, timeout_secs, abort, None).await
}

type BashOutputCallback = Arc<dyn Fn(String) + Send + Sync>;

async fn run_bash_with_callback(
    command: &str,
    cwd: &str,
    timeout_secs: Option<f64>,
    abort: Option<Arc<AtomicBool>>,
    on_output: Option<&BashOutputCallback>,
) -> Result<BashCapture, FileError> {
    let mut child = tokio::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| FileError::new(format!("failed to spawn bash: {e}")))?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    use tokio::io::AsyncReadExt;

    // Drain both pipes concurrently, racing the deadline and agent abort so
    // partial output is preserved without allowing a full stderr pipe to
    // deadlock stdout or cancellation.
    let deadline = timeout_secs
        .map(|secs| tokio::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
    let polling_deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(3_153_600_000);
    let mut so: Vec<u8> = Vec::new();
    let mut se: Vec<u8> = Vec::new();
    let mut so_eof = false;
    let mut se_eof = false;
    let mut timed_out = false;
    let mut aborted = false;
    let mut buf_so = [0u8; 8192];
    let mut buf_se = [0u8; 8192];
    while !(so_eof && se_eof) {
        if abort
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            aborted = true;
            let _ = child.kill().await;
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep_until(deadline.unwrap_or(polling_deadline)), if deadline.is_some() => {
                timed_out = true;
                let _ = child.kill().await;
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            result = stdout.read(&mut buf_so), if !so_eof => {
                match result {
                    Ok(0) => so_eof = true,
                    Ok(n) => {
                        so.extend_from_slice(&buf_so[..n]);
                        if let Some(on_output) = on_output {
                            on_output(combined_output(&so, &se));
                        }
                    }
                    Err(_) => so_eof = true,
                }
            }
            result = stderr.read(&mut buf_se), if !se_eof => {
                match result {
                    Ok(0) => se_eof = true,
                    Ok(n) => {
                        se.extend_from_slice(&buf_se[..n]);
                        if let Some(on_output) = on_output {
                            on_output(combined_output(&so, &se));
                        }
                    }
                    Err(_) => se_eof = true,
                }
            }
        }
    }
    if timed_out || aborted {
        let _ = child.kill().await;
    }
    let exit_code = child.wait().await.ok().and_then(|s| s.code());

    let output = combined_output(&so, &se);

    let (truncation, last_line_partial, last_line_bytes) = truncate_tail(&output);
    let mut truncation_message = String::new();
    if truncation.truncated {
        let start_line = truncation.total_lines - truncation.output_lines + 1;
        let end_line = truncation.total_lines;
        if last_line_partial {
            truncation_message = format!(
                "\n\n[Showing last {} of line {end_line} (line is {}). Full output truncated]",
                format_size(truncation.output_bytes),
                format_size(last_line_bytes)
            );
        } else if truncation.truncated_by == Some(super::truncate::TruncatedBy::Lines) {
            truncation_message = format!(
                "\n\n[Showing lines {start_line}-{end_line} of {}. Full output truncated]",
                truncation.total_lines
            );
        } else {
            truncation_message = format!(
                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output truncated]",
                truncation.total_lines,
                format_size(DEFAULT_MAX_BYTES)
            );
        }
    }

    Ok(BashCapture {
        output: truncation.content,
        exit_code: if timed_out || aborted {
            None
        } else {
            exit_code
        },
        truncated: truncation.truncated,
        truncation_message,
        timed_out,
        aborted,
    })
}

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut output = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&String::from_utf8_lossy(stderr));
    }
    output
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
    let output_callback = on_update.as_ref().map(|on_update| {
        let on_update = on_update.clone();
        let last_update_at = last_update_at.clone();
        Arc::new(move |output: String| {
            let should_emit = {
                let mut last_update_at = last_update_at.lock().unwrap();
                if last_update_at.elapsed() < Duration::from_millis(BASH_UPDATE_THROTTLE_MS) {
                    false
                } else {
                    *last_update_at = Instant::now();
                    true
                }
            };
            if should_emit {
                emit_bash_partial(&on_update, output, serde_json::Value::Null);
            }
        }) as BashOutputCallback
    });
    let capture = run_bash_with_callback(command, cwd, timeout, abort, output_callback.as_ref())
        .await
        .map_err(|e| e.to_string())?;

    if let Some(on_update) = &on_update {
        let details = if capture.truncated {
            serde_json::json!({"truncated": true})
        } else {
            serde_json::Value::Null
        };
        emit_bash_partial(on_update, capture.output.clone(), details);
    }

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
        Ok(AgentToolResult::from_tool_result_message(
            &ToolResultMessage::text(
                "bash",
                "bash",
                if output_text.is_empty() {
                    "(no output)".to_string()
                } else {
                    output_text
                },
                false,
            ),
        ))
    } else {
        Err(append_status(format!(
            "Command exited with code {}",
            capture.exit_code.unwrap_or(0)
        )))
    }
}

fn emit_bash_partial(on_update: &ToolUpdateCallback, output: String, details: serde_json::Value) {
    on_update(&AgentToolResult {
        content: vec![pi_ai::types::ContentBlock::text(output)],
        details,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
    });
}
