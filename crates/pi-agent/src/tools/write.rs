//! Write tool — port of `packages/agent/src/harness/tools/write.ts`.

use super::path_utils::resolve_tool_path;
use pi_ai::types::ToolResultMessage;
use std::sync::Arc;

pub async fn execute_write(
    tool_call_id: &str,
    path: &str,
    content: &str,
    cwd: &str,
) -> Result<ToolResultMessage, String> {
    execute_write_with_abort(tool_call_id, path, content, cwd, None).await
}

/// Write with the agent-loop abort flag attached. The filesystem operation is
/// still serialized by the shared mutation queue, while the checks match the
/// upstream boundaries immediately before and after the write.
pub async fn execute_write_with_abort(
    tool_call_id: &str,
    path: &str,
    content: &str,
    cwd: &str,
    abort: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<ToolResultMessage, String> {
    if crate::agent::is_aborted(abort.as_ref()) {
        return Err("Operation aborted".to_string());
    }
    let absolute = resolve_tool_path(cwd, path);
    let key = crate::harness::tools::resolve_mutation_key(cwd, path);
    let content = content.to_string();
    let path = path.to_string();
    let tool_call_id = tool_call_id.to_string();
    crate::harness::tools::with_file_mutation_queue(key, async move {
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }
        std::fs::create_dir_all(
            std::path::Path::new(&absolute)
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .map_err(|e| format!("Failed to create parent directories for {path}: {e}"))?;
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }
        std::fs::write(&absolute, &content).map_err(|e| format!("Failed to write {path}: {e}"))?;
        if crate::agent::is_aborted(abort.as_ref()) {
            return Err("Operation aborted".to_string());
        }
        Ok(ToolResultMessage::text(
            tool_call_id,
            "write",
            format!(
                "Successfully wrote {} bytes to {path}",
                content.encode_utf16().count()
            ),
            false,
        ))
    })
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use pi_ai::types::ContentBlock;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("pi-write-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn text(result: &ToolResultMessage) -> String {
        result
            .content()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn reports_javascript_utf16_length_for_unicode_content() {
        let dir = temp_dir("unicode");
        let result = execute_write(
            "write-1",
            "nested/file.txt",
            "é😀",
            &dir.display().to_string(),
        )
        .await
        .unwrap();
        assert!(text(&result).contains("Successfully wrote 3 bytes to nested/file.txt"));
        assert_eq!(
            fs::read_to_string(dir.join("nested/file.txt")).unwrap(),
            "é😀"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn aborts_before_mutating_the_file() {
        let dir = temp_dir("aborted");
        let abort = Arc::new(AtomicBool::new(true));
        let error = execute_write_with_abort(
            "write-1",
            "file.txt",
            "secret",
            &dir.display().to_string(),
            Some(abort),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Operation aborted");
        assert!(!dir.join("file.txt").exists());
        let _ = fs::remove_dir_all(dir);
    }
}
