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
pub fn resolve_mutation_key(cwd: &str, path: &str) -> String {
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        Path::new(cwd).join(path)
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => canonical.to_string_lossy().into_owned(),
        Err(_) => absolute.to_string_lossy().into_owned(),
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
        let mut reg = state.registration.lock().unwrap();
        let previous_reg = reg.take();
        let link = Arc::new(RegistrationLink {
            notified: tokio::sync::Notify::new(),
        });
        *reg = Some(link.clone());
        (previous_reg, link)
    };
    if let Some(prev) = previous_reg {
        prev.notified.notified().await;
    }

    // Chain this key's queue entry behind the current tail (scoped so the
    // lock guard drops before awaiting).
    let (previous_queue, link) = {
        let mut queues = state.queues.lock().unwrap();
        let previous_queue = queues.get(&key).cloned();
        let link = Arc::new(QueueLink {
            ready: tokio::sync::Notify::new(),
            finished: std::sync::atomic::AtomicBool::new(false),
        });
        queues.insert(key.clone(), link.clone());
        (previous_queue, link)
    };

    // Wait for the predecessor to finish.
    if let Some(prev) = previous_queue {
        if !prev.is_finished() {
            prev.ready.notified().await;
        }
    }
    // Release the registration chain so the NEXT mutation can register while
    // this one runs (upstream sets registration = registration.then(...)).
    reg_link.notified.notify_one();

    let result = f.await;
    link.finished
        .store(true, std::sync::atomic::Ordering::SeqCst);
    link.ready.notify_waiters();

    // Clean up the map entry if we are still the tail.
    {
        let mut queues = state.queues.lock().unwrap();
        if let Some(tail) = queues.get(&key) {
            if Arc::ptr_eq(tail, &link) {
                queues.remove(&key);
            }
        }
    }
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
                        let mut c = counter.lock().unwrap();
                        *c += 1;
                        let mut m = max.lock().unwrap();
                        *m = (*m).max(*c);
                        *c
                    };
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    *counter.lock().unwrap() -= 1;
                    current
                })
                .await
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*counter.lock().unwrap(), 0);
        assert_eq!(
            *max.lock().unwrap(),
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

    #[test]
    fn key_resolution_falls_back_to_absolute() {
        let cwd = std::env::temp_dir().to_string_lossy().into_owned();
        let key = resolve_mutation_key(&cwd, "no-such-file-xyz.md");
        assert!(key.contains("no-such-file-xyz.md"));
        assert!(std::path::Path::new(&key).is_absolute());
        assert_eq!(mutation_queue_key("/a/b", Some("/canon/b")), "/canon/b");
        assert_eq!(mutation_queue_key("/a/b", None), "/a/b");
    }
}
