//! Built-in execution tools — port of `packages/agent/src/harness/tools/`
//! (bash, read, write, edit; edit-diff + file-mutation-queue + image scanner
//! noted in TODO).

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod image;
pub mod path_utils;
pub mod read;
pub mod truncate;
pub mod validation;
pub mod write;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pi_ai::types::ToolResultMessage;
use pi_ai::types::{json_tool, Tool};

/// Final or partial result produced by a tool (upstream `AgentToolResult`).
#[derive(Debug, Clone, Default)]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<pi_ai::types::ContentBlock>,
    /// Arbitrary structured details for logs or UI rendering.
    pub details: serde_json::Value,
    /// Usage from the final tool execution itself, if available.
    pub usage: Option<pi_ai::types::Usage>,
    /// Names of tools introduced by this result and available from this
    /// transcript point onward.
    pub added_tool_names: Vec<String>,
    /// Hint that the agent should stop after the current tool batch. Early
    /// termination only happens when every finalized tool result in the batch
    /// sets this to true.
    pub terminate: bool,
}

impl AgentToolResult {
    /// Build a text-only result (upstream `createErrorToolResult` shape).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![pi_ai::types::ContentBlock::text(text)],
            details: serde_json::json!({}),
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        }
    }

    /// Convert a `ToolResultMessage` into an `AgentToolResult` (the content
    /// and details carry over; usage/addedToolNames are preserved).
    pub fn from_tool_result_message(result: &ToolResultMessage) -> Self {
        let ToolResultMessage::ToolResult {
            content,
            details,
            usage,
            added_tool_names,
            ..
        } = result;
        Self {
            content: content.clone(),
            details: details.clone().unwrap_or(serde_json::json!({})),
            usage: usage.clone(),
            added_tool_names: added_tool_names.clone().unwrap_or_default(),
            terminate: false,
        }
    }
}

/// Per-tool execution mode override (upstream `ToolExecutionMode`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

/// `onUpdate` callback: streams partial execution updates. Scoped to the
/// current `execute()` invocation.
pub type ToolUpdateCallback = Arc<dyn Fn(&AgentToolResult) + Send + Sync>;

pub type ToolExecuteFn = Arc<
    dyn Fn(
            String,
            serde_json::Value,
            Option<Arc<AtomicBool>>,
            Option<ToolUpdateCallback>,
        ) -> Pin<Box<dyn Future<Output = Result<AgentToolResult, String>> + Send>>
        + Send
        + Sync,
>;

/// `prepareArguments` compatibility shim: raw tool-call arguments before
/// schema validation. Must return an object that matches the parameters
/// schema.
pub type ToolPrepareArgumentsFn = Arc<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>;

/// A callable registered tool: unified schema + async execute (upstream
/// `AgentTool`).
#[derive(Clone)]
pub struct AgentTool {
    pub tool: Tool,
    /// Human-readable label for UI display.
    pub label: String,
    /// Optional compatibility shim for raw tool-call arguments before schema
    /// validation.
    pub prepare_arguments: Option<ToolPrepareArgumentsFn>,
    /// Execute the tool call. Throw on failure instead of encoding errors in
    /// `content`.
    pub execute: ToolExecuteFn,
    /// Per-tool execution mode override.
    pub execution_mode: Option<ToolExecutionMode>,
}

impl AgentTool {
    /// Build a tool from a schema + execute closure with the upstream shape.
    pub fn new(tool: Tool, label: impl Into<String>, execute: ToolExecuteFn) -> Self {
        Self {
            tool,
            label: label.into(),
            prepare_arguments: None,
            execute,
            execution_mode: None,
        }
    }

    pub fn with_prepare_arguments(mut self, f: ToolPrepareArgumentsFn) -> Self {
        self.prepare_arguments = Some(f);
        self
    }

    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }
}

/// Builds the unified `read` tool bound to a working directory.
pub fn read_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "read",
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                    "offset": {"type": "number", "description": "Line number to start reading from (1-indexed)"},
                    "limit": {"type": "number", "description": "Maximum number of lines to read"}
                },
                "required": ["path"]
            }),
        ),
        "Read file",
        Arc::new(move |tool_call_id, args, _signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "read: missing required argument path".to_string())?;
                let offset = args.get("offset").and_then(|v| v.as_f64());
                let limit = args.get("limit").and_then(|v| v.as_f64());
                let result = read::execute_read(&tool_call_id, path, offset, limit, &cwd).await?;
                Ok(AgentToolResult::from_tool_result_message(&result))
            })
        }),
    )
}

pub fn write_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "write",
            "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                    "content": {"type": "string", "description": "Content to write to the file"}
                },
                "required": ["path", "content"]
            }),
        ),
        "Write file",
        Arc::new(move |tool_call_id, args, _signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "write: missing required argument path".to_string())?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let result = write::execute_write(&tool_call_id, path, content, &cwd).await?;
                Ok(AgentToolResult::from_tool_result_message(&result))
            })
        }),
    )
}

pub fn edit_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "edit",
            "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
                                "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        ),
        "Edit file",
        Arc::new(move |tool_call_id, args, _signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "edit: missing required argument path".to_string())?;
                let edits = edit::extract_edits(&args)?;
                let result = edit::execute_edit(&tool_call_id, path, edits, &cwd).await?;
                Ok(AgentToolResult::from_tool_result_message(&result))
            })
        }),
    )
}

pub fn bash_tool(cwd: String) -> AgentTool {
    AgentTool::new(
        json_tool(
            "bash",
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). Optionally provide a timeout in seconds.",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Bash command to execute"},
                    "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
                },
                "required": ["command"]
            }),
        ),
        "Bash",
        Arc::new(move |_tool_call_id, args, _signal, _on_update| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "bash: missing required argument command".to_string())?;
                let timeout = args.get("timeout").and_then(|v| v.as_f64());
                let result = bash::execute_bash(command, timeout, &cwd).await?;
                Ok(AgentToolResult::from_tool_result_message(&result))
            })
        }),
    )
}
