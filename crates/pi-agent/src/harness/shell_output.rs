//! Shell output capture — port of
//! `packages/agent/src/harness/utils/shell-output.ts`.
//!
//! `execute_shell_with_capture` runs a command through an `ExecutionEnv`,
//! merging stdout+stderr through the tail-truncation + full-output-file
//! machinery: sanitize binary output, count lines/bytes, spill to a temp log
//! file once the tail window is exceeded, and report truncation progress to
//! `on_chunk`.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::harness::env::{
    ChunkHandler, CreateTempFileOptions, ExecutionEnv, ExecutionError, ExecutionErrorCode,
    FileContent, ShellExecOptions,
};
use crate::tools::truncate::{truncate_tail, TruncatedBy, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Truncation snapshot reported to callbacks (upstream `TruncationResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// Progress available to `on_chunk` callbacks (upstream
/// `ShellCaptureProgress`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShellCaptureProgress {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<String>,
    pub last_line_bytes: usize,
}

/// Chunk callback with progress access (upstream `onChunk?: (chunk,
/// getProgress)`).
pub type ChunkHandlerWithProgress =
    Arc<dyn Fn(&str, &Mutex<ShellCaptureProgress>) -> Result<(), String> + Send + Sync>;

/// Options for `execute_shell_with_capture` (upstream `ShellCaptureOptions`).
#[derive(Default)]
pub struct ShellCaptureOptions {
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub inherit_env: bool,
    pub timeout: Option<f64>,
    pub abort: Option<Arc<AtomicBool>>,
    pub on_chunk: Option<ChunkHandlerWithProgress>,
    /// Return shell execution failures with captured output instead of as a
    /// failed `Result`.
    pub return_execution_errors: bool,
}

impl std::fmt::Debug for ShellCaptureOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellCaptureOptions")
            .field("cwd", &self.cwd)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("return_execution_errors", &self.return_execution_errors)
            .finish_non_exhaustive()
    }
}

/// Result of a captured execution (upstream `ShellCaptureResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShellCaptureResult {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<String>,
    pub last_line_bytes: usize,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub execution_error: Option<ExecutionError>,
}

/// Sanitize binary output for safe display: keep tabs/newlines/carriage
/// returns, drop other control characters and Unicode noncharacters
/// (upstream `sanitizeBinaryOutput`).
pub fn sanitize_binary_output(input: &str) -> String {
    input
        .chars()
        .filter(|&ch| {
            let code = ch as u32;
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            if code <= 0x1f {
                return false;
            }
            if (0xfff9..=0xfffb).contains(&code) {
                return false;
            }
            true
        })
        .collect()
}

/// Keep the last `max_bytes` bytes of `text` on a UTF-8 boundary
/// (upstream `trimToLastUtf8Bytes`).
pub fn trim_to_last_utf8_bytes(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Accumulates tail + spill state during a captured execution.
#[derive(Debug, Default)]
struct CaptureAccumulator {
    tail_output: String,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    full_output_requested: bool,
}

/// Serialized full-output write operations (upstream `writeChain`).
enum WriteOp {
    CreateTempFile { initial: String },
    Append { text: String },
}

fn is_aborted_flag(flag: Option<&AtomicBool>) -> bool {
    flag.map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

fn create_progress(
    accum: &CaptureAccumulator,
    full_output_path: &Option<String>,
) -> ShellCaptureProgress {
    let (tail_truncation, last_line_partial, _) = truncate_tail(&accum.tail_output);
    let total_lines = accum.completed_lines + usize::from(accum.has_open_line);
    let truncated = total_lines > DEFAULT_MAX_LINES || accum.total_bytes > DEFAULT_MAX_BYTES;
    let truncated_by = if truncated {
        Some(
            tail_truncation
                .truncated_by
                .unwrap_or(if accum.total_bytes > DEFAULT_MAX_BYTES {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }),
        )
    } else {
        None
    };
    let tail_content = tail_truncation.content;
    ShellCaptureProgress {
        output: if truncated {
            tail_content.clone()
        } else {
            accum.tail_output.clone()
        },
        truncation: TruncationResult {
            content: tail_content,
            truncated,
            truncated_by,
            total_lines,
            total_bytes: accum.total_bytes,
            output_lines: tail_truncation.output_lines,
            output_bytes: tail_truncation.output_bytes,
            last_line_partial,
            first_line_exceeds_limit: false,
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        },
        full_output_path: full_output_path.clone(),
        last_line_bytes: accum.current_line_bytes,
    }
}

/// Execute `command` via `env.exec` while capturing tail output and spilling
/// to a full output file (upstream `executeShellWithCapture`).
pub async fn execute_shell_with_capture<E: ExecutionEnv>(
    env: &E,
    command: &str,
    options: &ShellCaptureOptions,
) -> Result<ShellCaptureResult, ExecutionError> {
    let max_output_bytes = DEFAULT_MAX_BYTES * 2;
    let accum = Arc::new(Mutex::new(CaptureAccumulator::default()));
    let writes: Arc<Mutex<Vec<WriteOp>>> = Arc::new(Mutex::new(Vec::new()));
    let maybe_full_path: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let on_chunk = options.on_chunk.clone();
    let chunk_cb: Option<ChunkHandler> = {
        let accum = accum.clone();
        let writes = writes.clone();
        let maybe_full_path = maybe_full_path.clone();
        let handler: ChunkHandler = Arc::new(move |chunk: &str| {
            let mut accum = accum.lock().unwrap_or_else(|error| error.into_inner());
            let mut writes = writes.lock().unwrap_or_else(|error| error.into_inner());
            let full_path = maybe_full_path
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let text = sanitize_binary_output(chunk).replace('\r', "");
            let text_bytes = text.len();
            let newline_count = text.split('\n').count() - 1;
            let last_newline = text.rfind('\n');
            match last_newline {
                Some(idx) => {
                    let trailing = &text[idx + 1..];
                    accum.total_bytes += text_bytes;
                    accum.completed_lines += newline_count;
                    accum.current_line_bytes = trailing.len();
                    accum.has_open_line = !trailing.is_empty();
                }
                None => {
                    if !text.is_empty() {
                        accum.total_bytes += text_bytes;
                        accum.completed_lines += newline_count;
                        accum.current_line_bytes += text_bytes;
                        accum.has_open_line = true;
                    }
                }
            }
            accum.tail_output += &text;
            let total_lines = accum.completed_lines + usize::from(accum.has_open_line);
            if (accum.total_bytes > DEFAULT_MAX_BYTES || total_lines > DEFAULT_MAX_LINES)
                && !accum.full_output_requested
            {
                accum.full_output_requested = true;
                writes.push(WriteOp::CreateTempFile {
                    initial: accum.tail_output.clone(),
                });
            } else if accum.full_output_requested {
                writes.push(WriteOp::Append { text: text.clone() });
            }
            accum.tail_output =
                trim_to_last_utf8_bytes(&accum.tail_output.clone(), max_output_bytes);
            // The capture machinery always runs; the optional user callback
            // is an observer (upstream `onChunk`).
            if let Some(progress_cb) = &on_chunk {
                let progress = create_progress(&accum, &full_path);
                progress_cb(&text, &Mutex::new(progress))?;
            }
            Ok(())
        });
        Some(handler)
    };

    let exec_opts = ShellExecOptions {
        cwd: options.cwd.clone(),
        env: options.env.clone(),
        inherit_env: options.inherit_env,
        timeout: options.timeout,
        abort: options.abort.clone(),
        on_stdout: chunk_cb.clone(),
        on_stderr: chunk_cb,
    };

    let result = env.exec(command, &exec_opts).await;

    // Capture errors surfaced from chunk handlers (upstream `captureError`).
    let capture_error = match &result {
        Err(e) if e.code == ExecutionErrorCode::CallbackError => Some(e.clone()),
        _ => None,
    };

    // Final ensure-full-output after the process settles.
    {
        let mut accum = accum.lock().unwrap_or_else(|error| error.into_inner());
        let mut writes = writes.lock().unwrap_or_else(|error| error.into_inner());
        let progress = create_progress(
            &accum,
            &maybe_full_path
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        if progress.truncation.truncated && !accum.full_output_requested {
            accum.full_output_requested = true;
            writes.push(WriteOp::CreateTempFile {
                initial: accum.tail_output.clone(),
            });
        }
    }

    // Flush serialized full-output writes.
    let mut full_output_path: Option<String> = None;
    {
        let ops: Vec<WriteOp> = writes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect();
        for op in ops {
            match op {
                WriteOp::CreateTempFile { initial } => {
                    let temp = env
                        .create_temp_file(CreateTempFileOptions {
                            prefix: Some("bash-"),
                            suffix: Some(".log"),
                        })
                        .await
                        .map_err(|e| {
                            ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string())
                        })?;
                    full_output_path = Some(temp.clone());
                    env.append_file(&temp, FileContent::Text(initial))
                        .await
                        .map_err(|e| {
                            ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string())
                        })?;
                }
                WriteOp::Append { text } => {
                    if let Some(path) = &full_output_path {
                        env.append_file(path, FileContent::Text(text))
                            .await
                            .map_err(|e| {
                                ExecutionError::new(ExecutionErrorCode::Unknown, e.to_string())
                            })?;
                    }
                }
            }
        }
    }
    *maybe_full_path
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = full_output_path.clone();

    // The upstream implementation waits for the serialized full-output write
    // chain before surfacing an observer/capture callback failure. This keeps
    // the diagnostic log complete even when the callback itself rejects a
    // chunk.
    if let Some(err) = capture_error {
        return Err(err);
    }

    let progress = create_progress(
        &accum.lock().unwrap_or_else(|error| error.into_inner()),
        &full_output_path,
    );
    let truncation = progress.truncation.clone();

    match result {
        Err(e) => {
            // Execution failure: mirror the upstream !result.ok branch.
            if is_aborted_flag(options.abort.as_deref()) {
                return Ok(ShellCaptureResult {
                    output: progress.output,
                    truncation: truncation.clone(),
                    full_output_path,
                    last_line_bytes: progress.last_line_bytes,
                    exit_code: None,
                    cancelled: true,
                    truncated: progress.truncation.truncated,
                    execution_error: None,
                });
            }
            if options.return_execution_errors {
                return Ok(ShellCaptureResult {
                    output: progress.output,
                    truncation: truncation.clone(),
                    full_output_path,
                    last_line_bytes: progress.last_line_bytes,
                    exit_code: None,
                    cancelled: false,
                    truncated: progress.truncation.truncated,
                    execution_error: Some(e),
                });
            }
            Err(e)
        }
        Ok(exec_result) => {
            let cancelled = is_aborted_flag(options.abort.as_deref());
            Ok(ShellCaptureResult {
                output: progress.output,
                truncation: truncation.clone(),
                full_output_path,
                last_line_bytes: progress.last_line_bytes,
                exit_code: if cancelled {
                    None
                } else {
                    Some(exec_result.exit_code)
                },
                cancelled,
                truncated: progress.truncation.truncated,
                execution_error: None,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::harness::env::StdExecutionEnv;

    fn temp_root() -> String {
        let base = std::env::temp_dir();
        let dir = base.join(format!("pi-shout-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn sanitize_binary_output_drops_control_chars() {
        assert_eq!(sanitize_binary_output("a\x00b\x09c\nd\x1be"), "ab\x09c\nde");
        assert_eq!(sanitize_binary_output("\u{fffa}plain\u{fffb}"), "plain");
    }

    #[test]
    fn trim_keeps_last_bytes_on_utf8_boundary() {
        let s = "héllo wörld is a fine phrase to split";
        let t = trim_to_last_utf8_bytes(s, 10);
        assert!(String::from_utf8(t.as_bytes().to_vec()).is_ok());
        assert!(s.ends_with(&t));
    }

    #[test]
    fn captures_small_output() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let result = execute_shell_with_capture(
                &env,
                "printf hello; printf ' world' >&2",
                &ShellCaptureOptions::default(),
            )
            .await
            .unwrap();
            assert!(result.output.contains("hello"));
            assert!(result.output.contains("world"));
            assert_eq!(result.exit_code, Some(0));
            assert!(!result.truncated);
        });
    }

    #[test]
    fn captures_large_output_to_full_output_file() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(root.clone());
            let result = execute_shell_with_capture(
                &env,
                "yes line | head -n 15000",
                &ShellCaptureOptions::default(),
            )
            .await
            .unwrap();
            assert!(result.truncated);
            let path = result.full_output_path.clone().expect("full output path");
            let full = std::fs::read_to_string(&path).unwrap();
            assert!(full.lines().count() > 10000);
            assert!(result.output.len() < full.len());
        });
    }

    #[test]
    fn returns_execution_errors_when_requested() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = temp_root();
            let env = StdExecutionEnv::new(format!("{root}/missing-cwd"));
            let result = execute_shell_with_capture(
                &env,
                "printf ok",
                &ShellCaptureOptions {
                    return_execution_errors: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(
                result.execution_error.as_ref().map(|e| e.code),
                Some(ExecutionErrorCode::SpawnError)
            );
        });
    }
}
