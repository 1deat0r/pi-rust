//! Built-in execution tools — port of `packages/agent/src/harness/tools/`
//! (bash, read, write, edit; edit-diff + file-mutation-queue + image scanner
//! noted in TODO).

pub mod bash;
pub mod edit;
pub mod path_utils;
pub mod read;
pub mod truncate;
pub mod write;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::types::ToolResultMessage;
use pi_ai::types::{json_tool, Tool};

pub type ToolExecuteFn = Arc<
    dyn Fn(serde_json::Value) -> Pin<Box<dyn Future<Output = Result<ToolResultMessage, String>> + Send>> + Send + Sync,
>;

/// A callable registered tool: unified schema + async execute.
pub struct AgentTool {
    pub tool: Tool,
    pub execute: ToolExecuteFn,
}

/// Builds the unified `read` tool bound to a working directory.
pub fn read_tool(cwd: String) -> AgentTool {
    AgentTool {
        tool: json_tool(
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
        execute: Arc::new(move |args| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "read: missing required argument path".to_string())?;
                let offset = args.get("offset").and_then(|v| v.as_f64());
                let limit = args.get("limit").and_then(|v| v.as_f64());
                read::execute_read("read", path, offset, limit, &cwd).await
            })
        }),
    }
}

pub fn write_tool(cwd: String) -> AgentTool {
    AgentTool {
        tool: json_tool(
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
        execute: Arc::new(move |args| {
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
                write::execute_write("write", path, content, &cwd).await
            })
        }),
    }
}

pub fn edit_tool(cwd: String) -> AgentTool {
    AgentTool {
        tool: json_tool(
            "edit",
            "Edit a file by replacing an exact string. The old_string must appear exactly once (or use replaceAll=true).",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                    "old_string": {"type": "string", "description": "Exact string to replace"},
                    "new_string": {"type": "string", "description": "Replacement string"},
                    "replace_all": {"type": "boolean", "description": "Replace all occurrences instead of erroring on duplicates"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        execute: Arc::new(move |args| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "edit: missing required argument path".to_string())?;
                let old = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "edit: missing required argument old_string".to_string())?;
                let new = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                let replace_all = args.get("replace_all").and_then(|v| v.as_bool());
                edit::execute_edit("edit", path, old, new, replace_all, &cwd).await
            })
        }),
    }
}

pub fn bash_tool(cwd: String) -> AgentTool {
    AgentTool {
        tool: json_tool(
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
        execute: Arc::new(move |args| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "bash: missing required argument command".to_string())?;
                let timeout = args.get("timeout").and_then(|v| v.as_f64());
                bash::execute_bash(command, timeout, &cwd).await
            })
        }),
    }
}
