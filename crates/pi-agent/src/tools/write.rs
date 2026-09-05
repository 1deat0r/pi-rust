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
    let key = crate::harness::tools::resolve_mutation_key(cwd, path)?;
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

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
    async fn creates_overwrites_and_reports_utf16_units_with_nested_parents() {
        let dir = temp_dir("create-overwrite");
        let cwd = dir.to_string_lossy();

        let created = execute_write("write-create", "nested/file.txt", "old", &cwd)
            .await
            .unwrap();
        assert_eq!(
            text(&created),
            "Successfully wrote 3 bytes to nested/file.txt"
        );
        assert_eq!(
            fs::read_to_string(dir.join("nested/file.txt")).unwrap(),
            "old"
        );

        let overwritten = execute_write("write-overwrite", "nested/file.txt", "界🙂", &cwd)
            .await
            .unwrap();
        assert_eq!(
            text(&overwritten),
            "Successfully wrote 3 bytes to nested/file.txt"
        );
        assert_eq!(
            fs::read_to_string(dir.join("nested/file.txt")).unwrap(),
            "界🙂"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reports_parent_and_write_errors_without_echoing_content() {
        let dir = temp_dir("errors");
        let secret = "synthetic-secret-that-must-not-leak";
        fs::write(dir.join("parent-is-file"), "occupied").unwrap();
        let parent_error = execute_write(
            "write-parent-error",
            "parent-is-file/child.txt",
            secret,
            &dir.to_string_lossy(),
        )
        .await
        .unwrap_err();
        assert!(parent_error.starts_with("Failed to resolve mutation path "));
        assert!(parent_error.contains("parent-is-file/child.txt"));
        assert!(!parent_error.contains(secret));

        let directory_error =
            execute_write("write-directory-error", ".", secret, &dir.to_string_lossy())
                .await
                .unwrap_err();
        assert!(directory_error.starts_with("Failed to write .:"));
        assert!(!directory_error.contains(secret));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_permission_denial_without_mutating_or_leaking_content() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("permission");
        let locked = dir.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let secret = "synthetic-secret-that-must-not-leak";
        let result = execute_write(
            "write-permission",
            "locked/file.txt",
            secret,
            &dir.to_string_lossy(),
        )
        .await;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.unwrap_err();
        assert!(error.starts_with("Failed to write locked/file.txt:"));
        assert!(!error.contains(secret));
        assert!(!locked.join("file.txt").exists());
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

    #[tokio::test]
    async fn aborts_while_waiting_for_the_same_path_mutation_queue() {
        let dir = temp_dir("queued-abort");
        let cwd = dir.to_string_lossy().into_owned();
        let key = crate::harness::tools::resolve_mutation_key(&cwd, "file.txt").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let holder = tokio::spawn(async move {
            crate::harness::tools::with_file_mutation_queue(key, async move {
                let _ = entered_tx.send(());
                let _ = release_rx.await;
            })
            .await;
        });
        entered_rx.await.unwrap();

        let abort = Arc::new(AtomicBool::new(false));
        let queued_abort = abort.clone();
        let queued_cwd = cwd.clone();
        let queued = tokio::spawn(async move {
            execute_write_with_abort(
                "write-queued",
                "file.txt",
                "must-not-be-written",
                &queued_cwd,
                Some(queued_abort),
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(!dir.join("file.txt").exists());
        abort.store(true, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        assert_eq!(queued.await.unwrap().unwrap_err(), "Operation aborted");
        assert!(!dir.join("file.txt").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_different_path_is_independent_of_a_blocked_mutation_queue() {
        let dir = temp_dir("independent-key");
        let cwd = dir.to_string_lossy().into_owned();
        let key = crate::harness::tools::resolve_mutation_key(&cwd, "blocked.txt").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let holder = tokio::spawn(async move {
            crate::harness::tools::with_file_mutation_queue(key, async move {
                let _ = entered_tx.send(());
                let _ = release_rx.await;
            })
            .await;
        });
        entered_rx.await.unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            execute_write("write-other", "other.txt", "independent", &cwd),
        )
        .await
        .expect("different mutation key must not wait")
        .unwrap();
        assert_eq!(text(&result), "Successfully wrote 11 bytes to other.txt");
        assert_eq!(
            fs::read_to_string(dir.join("other.txt")).unwrap(),
            "independent"
        );
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
