//! Bash tool — port of `packages/agent/src/harness/tools/bash.ts`
//! (execution + capture semantics; live `onUpdate` throttling is not carried
//! by the current loop and is noted in the TODO).

use super::truncate::{format_size, truncate_tail, DEFAULT_MAX_BYTES};
use crate::types::FileError;
use pi_ai::types::ToolResultMessage;

const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct BashCapture {
    pub output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub truncation_message: String,
    pub timed_out: bool,
}

/// Validates a bash timeout (seconds). Mirrors upstream `validateTimeout`.
pub fn validate_timeout(timeout: Option<f64>) -> Result<(), String> {
    match timeout {
        None => Ok(()),
        Some(v) => {
            if !v.is_finite() || v <= 0.0 {
                Err("Invalid timeout: must be a finite number of seconds".to_string())
            } else if v > MAX_TIMEOUT_SECONDS {
                Err(format!("Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"))
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

    // Drain both pipes concurrently, racing the deadline so partial output is
    // preserved when the command times out.
    let deadline = timeout_secs.map(|secs| tokio::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
    let mut so: Vec<u8> = Vec::new();
    let mut se: Vec<u8> = Vec::new();
    let mut so_eof = false;
    let mut se_eof = false;
    let mut timed_out = false;
    let mut buf_so = [0u8; 8192];
    let mut buf_se = [0u8; 8192];
    while !(so_eof && se_eof) {
        let mut saw_progress = false;
        if !so_eof {
            match deadline {
                Some(d) => {
                    let read = stdout.read(&mut buf_so);
                    tokio::pin!(read);
                    if tokio::select! {
                        _ = tokio::time::sleep_until(d) => { timed_out = true; true }
                        r = &mut read => {
                            match r {
                                Ok(0) => so_eof = true,
                                Ok(n) => { so.extend_from_slice(&buf_so[..n]); saw_progress = true; }
                                Err(_) => so_eof = true,
                            }
                            false
                        }
                    } {
                        break;
                    }
                }
                None => {
                    match stdout.read(&mut buf_so).await {
                        Ok(0) => so_eof = true,
                        Ok(n) => { so.extend_from_slice(&buf_so[..n]); saw_progress = true; },
                        Err(_) => so_eof = true,
                    }
                }
            }
        }
        if !se_eof {
            match deadline {
                Some(d) => {
                    let read = stderr.read(&mut buf_se);
                    tokio::pin!(read);
                    if tokio::select! {
                        _ = tokio::time::sleep_until(d) => { timed_out = true; true }
                        r = &mut read => {
                            match r {
                                Ok(0) => se_eof = true,
                                Ok(n) => { se.extend_from_slice(&buf_se[..n]); saw_progress = true; }
                                Err(_) => se_eof = true,
                            }
                            false
                        }
                    } {
                        break;
                    }
                }
                None => {
                    match stderr.read(&mut buf_se).await {
                        Ok(0) => se_eof = true,
                        Ok(n) => { se.extend_from_slice(&buf_se[..n]); saw_progress = true; },
                        Err(_) => se_eof = true,
                    }
                }
            }
        }
        if !saw_progress && timeout_secs.is_some() {
            // Sleep-bound child: still allow the deadline to fire.
            tokio::task::yield_now().await;
        }
    }
    if timed_out {
        let _ = child.kill().await;
    }
    let exit_code = child.wait().await.ok().and_then(|s| s.code());

    let mut output = String::from_utf8_lossy(&so).into_owned();
    if !se.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&String::from_utf8_lossy(&se));
    }

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
        exit_code: if timed_out { None } else { exit_code },
        truncated: truncation.truncated,
        truncation_message,
        timed_out,
    })
}

/// Execute handler for the agent tool: validates, runs, and renders the
/// tool-result message with upstream status semantics.
pub async fn execute_bash(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    validate_timeout(timeout)?;
    let capture = run_bash(command, cwd, timeout)
        .await
        .map_err(|e| e.to_string())?;

    let output_text = format!("{}{}", capture.output, capture.truncation_message);
    let append_status = |status: String| -> String {
        if output_text.is_empty() { status } else { format!("{output_text}\n\n{status}") }
    };

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
            if output_text.is_empty() { "(no output)".to_string() } else { output_text },
            false,
        ))
    } else {
        Err(append_status(format!("Command exited with code {}", capture.exit_code.unwrap_or(0))))
    }
}
