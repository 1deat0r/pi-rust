//! Write tool — port of `packages/agent/src/harness/tools/write.ts`.

use super::path_utils::resolve_tool_path;
use pi_ai::types::ToolResultMessage;

pub async fn execute_write(
    tool_call_id: &str,
    path: &str,
    content: &str,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    let absolute = resolve_tool_path(cwd, path);
    let key = crate::harness::tools::resolve_mutation_key(cwd, path);
    let content = content.to_string();
    let path = path.to_string();
    let tool_call_id = tool_call_id.to_string();
    crate::harness::tools::with_file_mutation_queue(key, async move {
        std::fs::create_dir_all(
            std::path::Path::new(&absolute)
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .map_err(|e| format!("Failed to create parent directories for {path}: {e}"))?;
        std::fs::write(&absolute, &content).map_err(|e| format!("Failed to write {path}: {e}"))?;
        Ok(ToolResultMessage::text(
            tool_call_id,
            "write",
            format!("Successfully wrote {} bytes to {path}", content.len()),
            false,
        ))
    })
    .await
}
