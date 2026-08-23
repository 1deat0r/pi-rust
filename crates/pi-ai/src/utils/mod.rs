//! Internal utilities — port of `packages/ai/src/utils/`.

pub mod estimate;
pub mod retry;

pub use estimate::estimate_context_tokens;
pub use retry::{
    is_retryable_assistant_error, retry_assistant_call, RetryCallbacks, RetryPolicy,
};

/// Serialize tests that mutate process-global environment variables so
/// parallel executions cannot race on the shared env (AWS_*, CLOUDFLARE_*,
/// GEMINI_API_KEY, ...). All env-mutating tests in the crate share this one
/// lock.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap()
}
