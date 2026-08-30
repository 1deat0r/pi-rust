//! Error-safe filesystem watching for reloadable Rust resources.
//!
//! The upstream runtime uses `fs.watch` and must attach an error handler at
//! construction time. This implementation uses a small polling watcher so the
//! Rust-only binary has no platform-specific watcher dependency while keeping
//! the same important semantics: an invalid path is reported synchronously,
//! later metadata/read failures are delivered to the error callback, and
//! shutdown is idempotent and never panics.

use std::any::Any;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

pub const FS_WATCH_RETRY_DELAY_MS: u64 = 5_000;
pub const FS_WATCH_RETRY_DELAY: Duration = Duration::from_millis(FS_WATCH_RETRY_DELAY_MS);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Changed,
    Removed,
}

pub type WatchListener = Arc<dyn Fn(WatchEvent) + Send + Sync + 'static>;
pub type WatchErrorHandler = Arc<dyn Fn(String) + Send + Sync + 'static>;

#[derive(Debug)]
pub struct FileWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FileWatcher {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn modified(path: &Path) -> Result<Option<SystemTime>, String> {
    match fs::metadata(path) {
        Ok(metadata) => metadata
            .modified()
            .map(Some)
            .map_err(|error| format!("read watcher metadata {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("watch {}: {error}", path.display())),
    }
}

fn panic_description(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Call user callbacks without allowing an unwinding callback to terminate
/// the detached polling worker. Node's watcher reports callback failures on
/// the event loop; the Rust equivalent is to route them to the error hook.
fn notify_listener(listener: &WatchListener, on_error: &WatchErrorHandler, event: WatchEvent) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
    if let Err(payload) = result {
        let message = format!(
            "filesystem watcher listener panicked: {}",
            panic_description(payload)
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_error(message)));
    }
}

fn notify_error(on_error: &WatchErrorHandler, error: String) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_error(error)));
}

/// Start a watcher for one file. The initial metadata lookup is performed
/// before spawning the worker, so a missing/unreadable path cannot create a
/// detached thread that fails later without an observable error.
pub fn watch_with_error_handler(
    path: impl AsRef<Path>,
    listener: WatchListener,
    on_error: WatchErrorHandler,
) -> Result<FileWatcher, String> {
    let path = path.as_ref().to_path_buf();
    let initial = match modified(&path) {
        Ok(Some(initial)) => Some(initial),
        Ok(None) => {
            let error = format!("watch {}: path does not exist", path.display());
            on_error(error.clone());
            return Err(error);
        }
        Err(error) => {
            on_error(error.clone());
            return Err(error);
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread = thread::Builder::new()
        .name("pi-fs-watch".to_string())
        .spawn(move || {
            let mut last = initial;
            let mut next_retry = None;
            while !stop_thread.load(Ordering::Acquire) {
                if let Some(deadline) = next_retry {
                    if SystemTime::now() < deadline {
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                    next_retry = None;
                }
                match modified(&path) {
                    Ok(current) => {
                        if current != last {
                            if current.is_none() {
                                notify_listener(&listener, &on_error, WatchEvent::Removed);
                            } else {
                                notify_listener(&listener, &on_error, WatchEvent::Changed);
                            }
                            last = current;
                        }
                    }
                    Err(error) => {
                        notify_error(&on_error, error);
                        // Keep the watcher alive after an asynchronous OS/file
                        // error. This is the regression-safe equivalent of an
                        // attached EventEmitter error listener.
                        next_retry = SystemTime::now().checked_add(FS_WATCH_RETRY_DELAY);
                    }
                }
                thread::sleep(POLL_INTERVAL);
            }
        })
        .map_err(|error| format!("start watcher: {error}"))?;
    Ok(FileWatcher {
        stop,
        thread: Some(thread),
    })
}

pub fn close_watcher(watcher: &mut Option<FileWatcher>) {
    if let Some(mut watcher) = watcher.take() {
        watcher.stop();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "pi-fs-watch-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn missing_path_is_reported_before_thread_creation() {
        let path = temp_path();
        let errors = Arc::new(AtomicUsize::new(0));
        let seen_errors = Arc::clone(&errors);
        let result = watch_with_error_handler(
            &path,
            Arc::new(|_| {}),
            Arc::new(move |_| {
                seen_errors.fetch_add(1, Ordering::Relaxed);
            }),
        );
        assert!(
            result.is_err(),
            "fs.watch-style setup rejects missing paths"
        );
        assert_eq!(errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn change_and_remove_events_are_delivered_and_stop_is_idempotent() {
        let path = temp_path();
        fs::write(&path, "one").unwrap();
        let changed = Arc::new(AtomicUsize::new(0));
        let removed = Arc::new(AtomicUsize::new(0));
        let changed_listener = Arc::clone(&changed);
        let removed_listener = Arc::clone(&removed);
        let mut watcher = watch_with_error_handler(
            &path,
            Arc::new(move |event| match event {
                WatchEvent::Changed => {
                    changed_listener.fetch_add(1, Ordering::Relaxed);
                }
                WatchEvent::Removed => {
                    removed_listener.fetch_add(1, Ordering::Relaxed);
                }
            }),
            Arc::new(|error| panic!("unexpected watcher error: {error}")),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(80));
        fs::write(&path, "two").unwrap();
        for _ in 0..30 {
            if changed.load(Ordering::Relaxed) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(changed.load(Ordering::Relaxed) > 0);
        fs::remove_file(&path).unwrap();
        for _ in 0..30 {
            if removed.load(Ordering::Relaxed) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(removed.load(Ordering::Relaxed) > 0);
        watcher.stop();
        watcher.stop();
    }

    #[test]
    fn panicking_listener_is_reported_without_killing_watcher_thread() {
        let path = temp_path();
        fs::write(&path, "one").unwrap();
        let errors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_errors = Arc::clone(&errors);
        let mut watcher = watch_with_error_handler(
            &path,
            Arc::new(|_| panic!("synthetic listener failure")),
            Arc::new(move |error| {
                seen_errors
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(error)
            }),
        )
        .unwrap();

        fs::write(&path, "two").unwrap();
        for _ in 0..30 {
            if !errors
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            errors
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            1
        );
        assert!(errors.lock().unwrap_or_else(|error| error.into_inner())[0]
            .contains("listener panicked"));
        watcher.stop();
        watcher.stop();
    }
}
