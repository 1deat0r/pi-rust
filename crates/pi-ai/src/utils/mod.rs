//! Internal utilities — port of `packages/ai/src/utils/`.

pub(crate) mod error_body;
pub mod estimate;
pub mod overflow;
pub mod retry;

/// Convert an HTTP response header map to the provider response representation
/// used by `StreamOptions.on_response`.
pub(crate) fn response_headers(
    headers: &reqwest::header::HeaderMap,
) -> std::collections::BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}

pub use estimate::estimate_context_tokens;
pub use overflow::{is_context_overflow, is_recoverable_length};
pub use retry::{is_retryable_assistant_error, retry_assistant_call, RetryCallbacks, RetryPolicy};

/// Serialize tests that mutate process-global environment variables so
/// parallel executions cannot race on the shared env (AWS_*, CLOUDFLARE_*,
/// GEMINI_API_KEY, ...). All env-mutating tests in the crate share this one
/// lock.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::response_headers;

    #[test]
    fn response_headers_preserves_text_and_omits_invalid_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-provider-trace"),
            reqwest::header::HeaderValue::from_static("trace-42"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("x-binary"),
            reqwest::header::HeaderValue::from_bytes(&[0xff]).expect("binary header value"),
        );

        let mapped = response_headers(&headers);
        assert_eq!(
            mapped.get("x-provider-trace").map(String::as_str),
            Some("trace-42")
        );
        assert!(!mapped.contains_key("x-binary"));
    }
}
