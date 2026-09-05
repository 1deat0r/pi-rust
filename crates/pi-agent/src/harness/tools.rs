//! File mutation queue + tool context — port of
//! `packages/agent/src/harness/tools/file-mutation-queue.ts` and
//! `tool-context.ts`.
//!
//! `with_file_mutation_queue` serializes file mutations targeting the same
//! canonical path so concurrent tool executions cannot interleave writes to a
//! file. The queue is keyed by the environment's canonical path (falling back
//! to the absolute path when canonicalization fails with not_found or
//! not_supported), mirroring upstream exactly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Canonical-path resolution used to key the mutation queue. This is the
/// synchronous counterpart of upstream `getMutationQueueKey`; callers provide
/// the canonicalized path when they have one, otherwise the absolute path is
/// used.
pub fn mutation_queue_key(absolute_path: &str, canonical_path: Option<&str>) -> String {
    canonical_path
        .map(|s| s.to_string())
        .unwrap_or_else(|| absolute_path.to_string())
}

/// Resolve a path to its canonical form where possible (stdlib realpath),
/// falling back to the absolute path when the file does not exist (matching
/// the upstream not_found fallback).
pub fn resolve_mutation_key(cwd: &str, path: &str) -> Result<String, String> {
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        Path::new(cwd).join(path)
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => Ok(canonical.to_string_lossy().into_owned()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::Unsupported
            ) =>
        {
            Ok(absolute.to_string_lossy().into_owned())
        }
        Err(error) => Err(format!(
            "Failed to resolve mutation path {}: {error}",
            absolute.display()
        )),
    }
}

struct QueueState {
    /// Per-key chain of linked queue entries.
    queues: Mutex<HashMap<String, Arc<QueueLink>>>,
    /// Registration chain: serializes the creation of new queue entries.
    registration: Mutex<Option<Arc<RegistrationLink>>>,
}

struct QueueLink {
    ready: tokio::sync::Notify,
    finished: std::sync::atomic::AtomicBool,
}

struct RegistrationLink {
    notified: tokio::sync::Notify,
}

struct RegistrationReleaseGuard {
    link: Arc<RegistrationLink>,
}

impl Drop for RegistrationReleaseGuard {
    fn drop(&mut self) {
        self.link.notified.notify_one();
    }
}

struct QueueReleaseGuard {
    state: &'static QueueState,
    key: String,
    link: Arc<QueueLink>,
}

impl Drop for QueueReleaseGuard {
    fn drop(&mut self) {
        self.link
            .finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.link.ready.notify_waiters();

        let mut queues = self
            .state
            .queues
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if queues
            .get(&self.key)
            .is_some_and(|tail| Arc::ptr_eq(tail, &self.link))
        {
            queues.remove(&self.key);
        }
    }
}

fn state() -> &'static QueueState {
    use std::sync::OnceLock;
    static STATE: OnceLock<QueueState> = OnceLock::new();
    STATE.get_or_init(|| QueueState {
        queues: Mutex::new(HashMap::new()),
        registration: Mutex::new(None),
    })
}

/// Run `f` after waiting for previous mutations of the same canonical key.
/// Mutations are serialized per key; the function runs once the previous
/// chained entry completes (upstream `withFileMutationQueue`).
pub async fn with_file_mutation_queue<T, F>(key: String, f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let state = state();
    // Serialize queue-registration globally (upstream registration chain).
    let (previous_reg, reg_link) = {
        let mut reg = state
            .registration
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_reg = reg.take();
        let link = Arc::new(RegistrationLink {
            notified: tokio::sync::Notify::new(),
        });
        *reg = Some(link.clone());
        (previous_reg, link)
    };
    let registration_release = RegistrationReleaseGuard {
        link: reg_link.clone(),
    };
    if let Some(prev) = previous_reg {
        prev.notified.notified().await;
    }

    // Chain this key's queue entry behind the current tail (scoped so the
    // lock guard drops before awaiting).
    let (previous_queue, link) = {
        let mut queues = state
            .queues
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_queue = queues.get(&key).cloned();
        let link = Arc::new(QueueLink {
            ready: tokio::sync::Notify::new(),
            finished: std::sync::atomic::AtomicBool::new(false),
        });
        queues.insert(key.clone(), link.clone());
        (previous_queue, link)
    };
    let queue_release = QueueReleaseGuard { state, key, link };

    // Wait for the predecessor to finish.
    if let Some(prev) = previous_queue {
        if !prev.is_finished() {
            prev.ready.notified().await;
        }
    }
    // Release the registration chain so the NEXT mutation can register while
    // this one runs (upstream sets registration = registration.then(...)).
    registration_release.link.notified.notify_one();

    let result = f.await;
    drop(queue_release);
    drop(registration_release);
    result
}

impl QueueLink {
    fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Tools require this filesystem/shell context (upstream `ExecutionToolContext`).
pub struct ExecutionToolContext {
    pub env: std::sync::Arc<dyn crate::fs::FileSystem + Send + Sync>,
}

/// How long mutations wait for an active file lock before giving up
/// (diagnostic helper; upstream relies on promise chains, so this is only
/// used by callers that want a bounded wait).
pub const MUTATION_POLL_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serializes_mutations_for_same_key() {
        let counter = Arc::new(Mutex::new(0u32));
        let max = Arc::new(Mutex::new(0u32));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let counter = counter.clone();
            let max = max.clone();
            handles.push(tokio::spawn(async move {
                with_file_mutation_queue("key".to_string(), async move {
                    let current = {
                        let mut c = counter.lock().unwrap_or_else(|error| error.into_inner());
                        *c += 1;
                        let mut m = max.lock().unwrap_or_else(|error| error.into_inner());
                        *m = (*m).max(*c);
                        *c
                    };
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    *counter.lock().unwrap_or_else(|error| error.into_inner()) -= 1;
                    current
                })
                .await
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            *counter.lock().unwrap_or_else(|error| error.into_inner()),
            0
        );
        assert_eq!(
            *max.lock().unwrap_or_else(|error| error.into_inner()),
            1,
            "never more than one mutation runs at a time"
        );
    }

    #[tokio::test]
    async fn different_keys_run_concurrently() {
        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for i in 0..4 {
            handles.push(tokio::spawn(async move {
                with_file_mutation_queue(format!("key-{i}"), async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                })
                .await
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Concurrent keys complete in ~100ms, not ~400ms.
        assert!(start.elapsed() < Duration::from_millis(350));
    }

    #[tokio::test]
    async fn cancelled_or_panicking_mutations_release_the_same_key() {
        let started = Arc::new(tokio::sync::Notify::new());
        let started_by_task = started.clone();
        let cancelled = tokio::spawn(async move {
            with_file_mutation_queue("cancel-key".to_string(), async move {
                started_by_task.notify_one();
                std::future::pending::<()>().await;
            })
            .await;
        });
        started.notified().await;
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        tokio::time::timeout(
            Duration::from_millis(250),
            with_file_mutation_queue("cancel-key".to_string(), async { 7 }),
        )
        .await
        .expect("cancelled mutation released queue");

        let panicked = tokio::spawn(async {
            with_file_mutation_queue("panic-key".to_string(), async {
                panic!("intentional mutation panic");
            })
            .await;
        });
        assert!(panicked.await.unwrap_err().is_panic());
        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(250),
                with_file_mutation_queue("panic-key".to_string(), async { 9 }),
            )
            .await
            .expect("panicking mutation released queue"),
            9
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_aliases_share_the_canonical_queue() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "pi-mutation-symlink-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("target.txt"), "hello").unwrap();
        symlink(root.join("target.txt"), root.join("alias.txt")).unwrap();
        let target_key = resolve_mutation_key(
            &root.display().to_string(),
            &root.join("target.txt").display().to_string(),
        )
        .unwrap();
        let alias_key = resolve_mutation_key(
            &root.display().to_string(),
            &root.join("alias.txt").display().to_string(),
        )
        .unwrap();
        assert_eq!(target_key, alias_key);

        let order = Arc::new(Mutex::new(Vec::new()));
        let first_order = order.clone();
        let first = tokio::spawn(async move {
            with_file_mutation_queue(target_key, async move {
                first_order.lock().unwrap().push("target:start");
                tokio::time::sleep(Duration::from_millis(30)).await;
                first_order.lock().unwrap().push("target:end");
            })
            .await;
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second_order = order.clone();
        let second = tokio::spawn(async move {
            with_file_mutation_queue(alias_key, async move {
                second_order.lock().unwrap().push("alias:start");
                second_order.lock().unwrap().push("alias:end");
            })
            .await;
        });
        first.await.unwrap();
        second.await.unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            vec!["target:start", "target:end", "alias:start", "alias:end"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn key_resolution_falls_back_to_absolute() {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let key = resolve_mutation_key(&cwd, "no-such-file-xyz.md").unwrap();
        assert!(key.contains("no-such-file-xyz.md"));
        assert!(std::path::Path::new(&key).is_absolute());
        assert_eq!(mutation_queue_key("/a/b", Some("/canon/b")), "/canon/b");
        assert_eq!(mutation_queue_key("/a/b", None), "/a/b");
    }

    #[test]
    fn key_resolution_propagates_non_fallback_errors() {
        let root = std::env::temp_dir().join(format!(
            "pi-mutation-not-directory-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), "not a directory").unwrap();

        let error = resolve_mutation_key(&root.display().to_string(), "file/child")
            .expect_err("not-directory canonicalization must not fall back");
        assert!(error.contains("Failed to resolve mutation path"));
        assert!(error.contains("file/child"));
        let _ = std::fs::remove_dir_all(root);
    }
}
