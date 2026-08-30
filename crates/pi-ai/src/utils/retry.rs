//! Assistant-call retry policy and classification — port of
//! `packages/ai/src/utils/retry.ts` (bounded exponential backoff + the
//! retryable/non-retryable error classifier).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tokio::time::Instant;

use crate::types::{AssistantMessage, StopReason};

/// Retry policy: bounded attempts with exponential backoff
/// (`baseDelayMs * 2^(attempt-1)`). Mirrors `settings.retry`
/// (`enabled`, `maxRetries`, `baseDelayMs`) in coding-agent.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries). The initial call never counts as a retry.
    pub max_retries: u32,
    /// Base delay in ms. Per-attempt delay is `baseDelayMs * 2^(attempt-1)` before jitter.
    pub base_delay_ms: u64,
}

/// Callback invoked before each backoff sleep.
pub type OnRetryScheduled<'a> = Box<dyn Fn(u32, u32, u64, String) + Send + Sync + 'a>;
/// Callback invoked after the backoff sleep, before the retried call starts.
pub type OnRetryAttemptStart<'a> = Box<dyn Fn() + Send + Sync + 'a>;
/// Callback invoked once when the retry loop ends.
pub type OnRetryFinished<'a> = Box<dyn Fn(bool, u32, Option<String>) + Send + Sync + 'a>;

/// Optional callbacks emitted by [`retry_assistant_call`] around each retry.
/// Upstream callbacks may await async work; the port invokes them
/// synchronously (documented divergence — no async work exists in the current
/// consumers).
#[derive(Default)]
pub struct RetryCallbacks<'a> {
    /// Emitted before the backoff sleep of each retry attempt (1-indexed).
    pub on_retry_scheduled: Option<OnRetryScheduled<'a>>,
    /// Emitted after the backoff sleep, immediately before the retried call starts.
    pub on_retry_attempt_start: Option<OnRetryAttemptStart<'a>>,
    /// Emitted once when the loop ends: success if a later call completed normally.
    pub on_retry_finished: Option<OnRetryFinished<'a>>,
}

struct RetrySleepAbort;

/// Sleep for `ms` milliseconds, waking early when `signal` flips to aborted.
/// The port polls the flag on a 25 ms granularity (upstream uses an
/// AbortSignal event; the harness signal is a shared atomic flag).
async fn sleep(ms: u64, signal: Option<&Arc<AtomicBool>>) -> Result<(), RetrySleepAbort> {
    if signal.is_some_and(|s| s.load(Ordering::SeqCst)) {
        return Err(RetrySleepAbort);
    }
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
        if signal.is_some_and(|s| s.load(Ordering::SeqCst)) {
            return Err(RetrySleepAbort);
        }
    }
}

#[allow(clippy::expect_used)] // invariant: static retry pattern literals compile
fn build_pattern(patterns: &[&str]) -> Regex {
    regex::RegexBuilder::new(&format!("({})", patterns.join("|")))
        .case_insensitive(true)
        .build()
        .expect("static retry patterns must compile case-insensitively")
}

const NON_RETRYABLE_PROVIDER_LIMIT_PATTERNS: &[&str] = &[
    // OpenCode Go/free-tier limits returned as 429 JSON error types by
    // OpenCode's Zen API.
    "GoUsageLimitError",
    "FreeUsageLimitError",
    // OpenCode Go subscription-limit text.
    "Monthly usage limit reached",
    "available balance",
    // Generic quota/budget/billing exhaustion.
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
];

const RETRYABLE_PROVIDER_PATTERNS: &[&str] = &[
    // Generic provider load, HTTP status, and server-side transient failures.
    "overloaded",
    "rate.?limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    // Wrapper/provider text for transient upstream failures.
    "provider.?returned.?error",
    "exceeded request buffer limit while retrying upstream",
    // Network, proxy, and fetch transport failures.
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    // WebSocket transports can report close/error text instead of HTTP/fetch text.
    "websocket.?closed",
    "websocket.?error",
    // Premature stream endings from SDKs and transports.
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    // Provider-requested retry delay cap failures flow through the outer
    // retry policy so callers can surface/abort the backoff (#1123).
    "retry delay",
    // Explicit retry guidance emitted mid-stream by OpenAI Responses and
    // Bedrock stream exceptions (#6019).
    "you can retry your request",
    "try your request again",
    "please retry your request",
    // gRPC based providers (e.g. NVIDIA NIM).
    "ResourceExhausted",
];

thread_local! {
    static NON_RETRYABLE_PATTERN: Regex =
        build_pattern(NON_RETRYABLE_PROVIDER_LIMIT_PATTERNS);
    static RETRYABLE_PATTERN: Regex =
        build_pattern(RETRYABLE_PROVIDER_PATTERNS);
}

/// Classifies whether a failed assistant message looks like a transient
/// provider or transport error, so callers can decide if the last assistant
/// turn should be restarted. This does not implement retry policy.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason() != Some(StopReason::Error) {
        return false;
    }
    let Some(error_message) = message.error_message() else {
        return false;
    };
    let non_retryable = NON_RETRYABLE_PATTERN.with(|p| p.is_match(error_message));
    if non_retryable {
        return false;
    }
    RETRYABLE_PATTERN.with(|p| p.is_match(error_message))
}

/// Run a single assistant-producing call with bounded retry on transient
/// errors. Behavior mirrors upstream `retryAssistantCall` exactly:
/// - success returned immediately;
/// - aborts are terminal and never retried (reported as unsuccessful only if
///   a retry had already been scheduled);
/// - non-retryable errors returned immediately;
/// - otherwise retries up to `maxRetries` with exponential backoff.
///
/// When `policy` is `None` or disabled, the first response is returned
/// unchanged.
#[allow(clippy::expect_used)] // invariant: retry scheduled only after a first failure
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<&RetryPolicy>,
    signal: Option<&Arc<AtomicBool>>,
    callbacks: Option<&RetryCallbacks<'_>>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let max_attempts = policy
        .filter(|p| p.enabled)
        .map(|p| p.max_retries)
        .unwrap_or(0);
    let base_delay_ms = policy.map(|p| p.base_delay_ms).unwrap_or(0);

    let mut attempt = 0u32;
    let mut last_retry: Option<(u32, String)> = None;

    loop {
        let response = produce().await;

        // Abort: terminal but not successful. Never retry an aborted message.
        if response.stop_reason() == Some(StopReason::Aborted) {
            if let Some((n, _)) = last_retry {
                emit_finished(callbacks, false, n, None);
            }
            return response;
        }

        // Success: non-error, non-abort responses return as-is.
        if response.stop_reason() != Some(StopReason::Error) {
            if let Some((n, _)) = last_retry {
                emit_finished(callbacks, true, n, None);
            }
            return response;
        }

        // Non-retryable, or budget exhausted: return the final error message.
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((n, _)) = last_retry {
                emit_finished(callbacks, false, n, response.error_message());
            }
            return response;
        }

        attempt += 1;
        last_retry = Some((
            attempt,
            response
                .error_message()
                .unwrap_or("Unknown error")
                .to_string(),
        ));
        let delay_ms = base_delay_ms.saturating_mul(1u64 << (attempt.saturating_sub(1).min(30)));
        if let Some(cb) = callbacks.and_then(|c| c.on_retry_scheduled.as_ref()) {
            cb(
                attempt,
                max_attempts,
                delay_ms,
                last_retry
                    .as_ref()
                    .expect("retry scheduled only after a first failure")
                    .1
                    .clone(),
            );
        }

        // Normalize aborts during retry backoff to the same AssistantMessage
        // shape as provider stream aborts.
        match sleep(delay_ms, signal).await {
            Ok(()) => {}
            Err(RetrySleepAbort) => {
                if let Some((n, _)) = last_retry {
                    emit_finished(
                        callbacks,
                        false,
                        n,
                        last_retry.as_ref().map(|(_, m)| m.as_str()),
                    );
                }
                let mut response = response;
                response.set_stop_reason(StopReason::Aborted);
                let AssistantMessage::Assistant { error_message, .. } = &mut response;
                *error_message = None;
                return response;
            }
        }
        if let Some(cb) = callbacks.and_then(|c| c.on_retry_attempt_start.as_ref()) {
            cb();
        }
    }
}

fn emit_finished(
    callbacks: Option<&RetryCallbacks<'_>>,
    success: bool,
    attempt: u32,
    final_error: Option<&str>,
) {
    if let Some(cb) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
        cb(success, attempt, final_error.map(|s| s.to_string()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::providers::{faux_assistant_message, FauxAssistantOptions};

    fn msg(text: &str) -> AssistantMessage {
        faux_assistant_message(
            vec![crate::providers::faux_text(text)],
            FauxAssistantOptions::default(),
        )
    }

    fn err_msg(text: &str) -> AssistantMessage {
        faux_assistant_message(
            vec![],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some(text.to_string()),
            },
        )
    }

    fn aborted_msg() -> AssistantMessage {
        faux_assistant_message(
            vec![],
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Aborted),
                error_message: None,
            },
        )
    }

    fn policy(enabled: bool, max_retries: u32, base_delay_ms: u64) -> RetryPolicy {
        RetryPolicy {
            enabled,
            max_retries,
            base_delay_ms,
        }
    }

    #[test]
    fn matches_explicit_provider_retry_guidance() {
        let openai =
            "An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists. Please include the request ID req_******** in your message.";
        let bedrock = r#"{"message":"The system encountered an unexpected error during processing. Try your request again."}"#;
        let nvidia = "ResourceExhausted: Worker local total request limit reached (288/48)";
        assert!(is_retryable_assistant_error(&err_msg(openai)));
        assert!(is_retryable_assistant_error(&err_msg(bedrock)));
        assert!(is_retryable_assistant_error(&err_msg(nvidia)));
    }

    #[test]
    fn matches_bun_fetch_socket_drop_wording() {
        let msg = "The socket connection was closed unexpectedly. For more information, pass `verbose: true` in the second argument to fetch()";
        assert!(is_retryable_assistant_error(&err_msg(msg)));
    }

    #[test]
    fn matches_upstream_request_buffer_exhaustion_wording() {
        assert!(is_retryable_assistant_error(&err_msg(
            "Error: exceeded request buffer limit while retrying upstream"
        )));
    }

    #[test]
    fn matches_dns_transport_failure_wording() {
        for msg in [
            "The pending stream has been canceled (caused by: getaddrinfo ENOTFOUND bedrock-runtime.us-east-1.amazonaws.com)",
            "connect ENOTFOUND api.example.com",
            "EAI_AGAIN api.example.com",
            "getaddrinfo failed for api.example.com",
        ] {
            assert!(is_retryable_assistant_error(&err_msg(msg)), "expected retryable: {msg}");
        }
    }

    #[test]
    fn matches_openai_responses_streams_that_end_before_terminal_events() {
        assert!(is_retryable_assistant_error(&err_msg(
            "OpenAI Responses stream ended before a terminal response event"
        )));
    }

    #[test]
    fn keeps_provider_limit_errors_non_retryable() {
        assert!(!is_retryable_assistant_error(&err_msg(
            "429 quota exceeded"
        )));
    }

    #[test]
    fn classifies_assistant_error_messages() {
        assert!(is_retryable_assistant_error(&err_msg("overloaded_error")));
        assert!(is_retryable_assistant_error(&err_msg(
            "524 status code (no body)"
        )));
        // Non-error responses are never retryable.
        assert!(!is_retryable_assistant_error(&msg("not an error")));
    }

    #[tokio::test]
    async fn returns_a_successful_response_immediately_without_retrying() {
        let mut calls = 0;
        let res = retry_assistant_call(
            || {
                calls += 1;
                async { msg("ok") }
            },
            Some(&policy(true, 3, 0)),
            None,
            None,
        )
        .await;
        assert_eq!(res.content(), &[crate::providers::faux_text("ok")]);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn does_not_retry_an_aborted_message() {
        let mut calls = 0;
        let scheduled = std::sync::atomic::AtomicU32::new(0);
        let cb = RetryCallbacks {
            on_retry_scheduled: Some(Box::new(|_, _, _, _| {
                scheduled.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                async { aborted_msg() }
            },
            Some(&policy(true, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.stop_reason(), Some(StopReason::Aborted));
        assert_eq!(calls, 1);
        assert_eq!(scheduled.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn does_not_retry_a_non_retryable_error() {
        let mut calls = 0;
        let (scheduled, finished) = (
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicU32::new(0),
        );
        let cb = RetryCallbacks {
            on_retry_scheduled: Some(Box::new(|_, _, _, _| {
                scheduled.fetch_add(1, Ordering::SeqCst);
            })),
            on_retry_finished: Some(Box::new(|_, _, _| {
                finished.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                async { err_msg("insufficient_quota") }
            },
            Some(&policy(true, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.stop_reason(), Some(StopReason::Error));
        assert_eq!(calls, 1);
        assert_eq!(scheduled.load(Ordering::SeqCst), 0);
        assert_eq!(finished.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retries_a_transient_error_up_to_max_retries_then_returns_final_error() {
        let mut calls = 0;
        let (scheduled, finished, finished_args) = (
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicU32::new(0),
            std::sync::Mutex::new(None),
        );
        let cb = RetryCallbacks {
            on_retry_scheduled: Some(Box::new(|_, _, _, _| {
                scheduled.fetch_add(1, Ordering::SeqCst);
            })),
            on_retry_finished: Some(Box::new(|ok, attempt, final_error| {
                finished.fetch_add(1, Ordering::SeqCst);
                *finished_args
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some((ok, attempt, final_error.map(|s| s.to_string())));
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                async { err_msg("terminated") }
            },
            Some(&policy(true, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.stop_reason(), Some(StopReason::Error));
        assert_eq!(calls, 4); // 1 initial + 3 retries
        assert_eq!(scheduled.load(Ordering::SeqCst), 3);
        let args = finished_args
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap();
        assert_eq!(args, (false, 3, Some("terminated".to_string())));
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stops_retrying_once_a_call_succeeds() {
        let mut calls = 0;
        let finished_args = std::sync::Mutex::new(None);
        let cb = RetryCallbacks {
            on_retry_finished: Some(Box::new(|ok, attempt, _| {
                *finished_args
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some((ok, attempt));
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                let n = calls;
                async move {
                    if n < 3 {
                        err_msg("terminated")
                    } else {
                        msg("recovered")
                    }
                }
            },
            Some(&policy(true, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.content(), &[crate::providers::faux_text("recovered")]);
        assert_eq!(calls, 3);
        assert_eq!(
            *finished_args
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            Some((true, 2))
        );
    }

    #[tokio::test]
    async fn reports_an_aborted_retried_call_as_unsuccessful() {
        let mut calls = 0;
        let finished_args = std::sync::Mutex::new(None);
        let cb = RetryCallbacks {
            on_retry_finished: Some(Box::new(|ok, attempt, _| {
                *finished_args
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some((ok, attempt));
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                let n = calls;
                async move {
                    if n == 1 {
                        err_msg("terminated")
                    } else {
                        aborted_msg()
                    }
                }
            },
            Some(&policy(true, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.stop_reason(), Some(StopReason::Aborted));
        assert_eq!(calls, 2);
        assert_eq!(
            *finished_args
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            Some((false, 1))
        );
    }

    #[tokio::test]
    async fn does_not_retry_when_policy_is_disabled() {
        let mut calls = 0;
        let (scheduled, finished) = (
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicU32::new(0),
        );
        let cb = RetryCallbacks {
            on_retry_scheduled: Some(Box::new(|_, _, _, _| {
                scheduled.fetch_add(1, Ordering::SeqCst);
            })),
            on_retry_finished: Some(Box::new(|_, _, _| {
                finished.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                async { err_msg("terminated") }
            },
            Some(&policy(false, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.stop_reason(), Some(StopReason::Error));
        assert_eq!(calls, 1);
        assert_eq!(scheduled.load(Ordering::SeqCst), 0);
        assert_eq!(finished.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn emits_on_retry_attempt_start_after_backoff_before_each_retried_call() {
        let events = std::sync::Mutex::new(Vec::new());
        let mut calls = 0;
        let cb = RetryCallbacks {
            on_retry_scheduled: Some(Box::new(|attempt, _, _, _| {
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(format!("retry:{attempt}"));
            })),
            on_retry_attempt_start: Some(Box::new(|| {
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push("attempt-start".to_string());
            })),
            ..Default::default()
        };
        let res = retry_assistant_call(
            || {
                calls += 1;
                let n = calls;
                events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(format!("produce:{}", n - 1));
                async move {
                    if n < 3 {
                        err_msg("terminated")
                    } else {
                        msg("recovered")
                    }
                }
            },
            Some(&policy(true, 3, 0)),
            None,
            Some(&cb),
        )
        .await;
        assert_eq!(res.content(), &[crate::providers::faux_text("recovered")]);
        assert_eq!(
            events
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            &[
                "produce:0".to_string(),
                "retry:1".to_string(),
                "attempt-start".to_string(),
                "produce:1".to_string(),
                "retry:2".to_string(),
                "attempt-start".to_string(),
                "produce:2".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn aborts_backoff_sleep_via_signal_returns_aborted_message_and_reports_failure() {
        let signal = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let finished_args = Arc::new(std::sync::Mutex::new(None));
        let finished_args_cb = finished_args.clone();
        let cb = RetryCallbacks {
            on_retry_finished: Some(Box::new(move |ok, attempt, final_error| {
                *finished_args_cb
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some((ok, attempt, final_error.map(|s| s.to_string())));
            })),
            ..Default::default()
        };
        let policy = policy(true, 5, 10_000);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let signal_for_task = signal.clone();
        let calls_for_task = calls.clone();
        let task = tokio::spawn(async move {
            let res = retry_assistant_call(
                move || {
                    calls_for_task.fetch_add(1, Ordering::SeqCst);
                    async { err_msg("terminated") }
                },
                Some(&policy),
                Some(&signal_for_task),
                Some(&cb),
            )
            .await;
            let _ = tx.send(res);
        });
        // Let one error call resolve and the first backoff sleep start, then abort.
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        signal.store(true, Ordering::SeqCst);
        let res = rx.await.unwrap();
        assert_eq!(res.stop_reason(), Some(StopReason::Aborted));
        assert_eq!(res.error_message(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *finished_args
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            Some((false, 1, Some("terminated".to_string())))
        );
        let _ = task.await;
    }
}
