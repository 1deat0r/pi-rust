//! Bounded HTTP transport for idempotent management requests.
//!
//! This is the Rust counterpart of `utils/management-http.ts`.  It is kept
//! deliberately separate from model/agent requests: version checks, model
//! catalogs, and downloads may be retried at the transport boundary, while a
//! semantic agent request must be retried by its caller with its own
//! idempotency rules.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{header::HeaderMap, Client, Method, RequestBuilder, Response, Url};

pub const RETRYABLE_STATUS_CODES: [reqwest::StatusCode; 7] = [
    reqwest::StatusCode::REQUEST_TIMEOUT,
    reqwest::StatusCode::TOO_EARLY,
    reqwest::StatusCode::TOO_MANY_REQUESTS,
    reqwest::StatusCode::INTERNAL_SERVER_ERROR,
    reqwest::StatusCode::BAD_GATEWAY,
    reqwest::StatusCode::SERVICE_UNAVAILABLE,
    reqwest::StatusCode::GATEWAY_TIMEOUT,
];

const DEFAULT_MAX_RETRIES: usize = 2;
// The upstream helper retries immediately. A caller that needs a slower
// policy can override this field, but the shared default must not introduce a
// hidden delay into startup/catalog management requests.
const DEFAULT_RETRY_DELAY: Duration = Duration::ZERO;
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A request specification that can be rebuilt for every retry.
///
/// Keeping the method, URL, headers, and body as owned values avoids trying
/// to clone a consumed `reqwest::RequestBuilder` and makes the retry boundary
/// safe for future POST management endpoints whose body is still idempotent
/// by contract.
#[derive(Debug, Clone)]
pub struct ManagementRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
}

impl ManagementRequest {
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
        }
    }

    pub fn get(url: Url) -> Self {
        Self::new(Method::GET, url)
    }

    pub fn into_builder(&self, client: &Client) -> RequestBuilder {
        let mut builder = client.request(self.method.clone(), self.url.clone());
        if !self.headers.is_empty() {
            builder = builder.headers(self.headers.clone());
        }
        if let Some(body) = &self.body {
            builder = builder.body(body.clone());
        }
        builder
    }
}

#[derive(Debug, Clone)]
pub struct FetchRetryOptions {
    /// Number of additional attempts after the initial request.
    pub max_retries: usize,
    /// Retry transient HTTP statuses in addition to transport failures.
    pub retry_on_status: bool,
    /// Overall time budget shared by every attempt and retry delay.
    pub timeout: Option<Duration>,
    /// Timeout for one request attempt. A new timeout is created per attempt.
    pub attempt_timeout: Option<Duration>,
    /// Delay between attempts. Management requests do not use unbounded
    /// exponential backoff; the caller owns the total time budget.
    pub retry_delay: Duration,
}

impl Default for FetchRetryOptions {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            retry_on_status: true,
            timeout: None,
            attempt_timeout: None,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }
}

impl FetchRetryOptions {
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchRetryError {
    #[error("management HTTP request cancelled")]
    Cancelled,
    #[error("management HTTP request timed out")]
    TimedOut,
    #[error("management HTTP attempt timed out")]
    AttemptTimedOut,
    #[error("management HTTP transport failed: {0}")]
    Transport(#[source] reqwest::Error),
}

fn is_cancelled(signal: Option<&Arc<std::sync::atomic::AtomicBool>>) -> bool {
    signal.is_some_and(|signal| signal.load(std::sync::atomic::Ordering::Acquire))
}

async fn cancellation_future(signal: Option<Arc<std::sync::atomic::AtomicBool>>) {
    let Some(signal) = signal else {
        std::future::pending::<()>().await;
        return;
    };
    while !signal.load(std::sync::atomic::Ordering::Acquire) {
        tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
    }
}

async fn send_attempt(
    client: &Client,
    request: &ManagementRequest,
    options: &FetchRetryOptions,
    signal: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Response, FetchRetryError> {
    if is_cancelled(signal.as_ref()) {
        return Err(FetchRetryError::Cancelled);
    }
    let send = request.into_builder(client).send();
    let send = async {
        tokio::select! {
            result = send => result.map_err(FetchRetryError::Transport),
            _ = cancellation_future(signal) => Err(FetchRetryError::Cancelled),
        }
    };
    match options.attempt_timeout {
        Some(timeout) if !timeout.is_zero() => tokio::time::timeout(timeout, send)
            .await
            .unwrap_or(Err(FetchRetryError::AttemptTimedOut)),
        _ => send.await,
    }
}

async fn retry_delay(
    duration: Duration,
    signal: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), FetchRetryError> {
    if duration.is_zero() {
        return if is_cancelled(signal.as_ref()) {
            Err(FetchRetryError::Cancelled)
        } else {
            Ok(())
        };
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => {
            if is_cancelled(signal.as_ref()) {
                Err(FetchRetryError::Cancelled)
            } else {
                Ok(())
            }
        }
        _ = cancellation_future(signal.clone()) => Err(FetchRetryError::Cancelled),
    }
}

async fn fetch_loop(
    client: &Client,
    request: &ManagementRequest,
    options: FetchRetryOptions,
    signal: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Response, FetchRetryError> {
    for attempt in 0..=options.max_retries {
        if is_cancelled(signal.as_ref()) {
            return Err(FetchRetryError::Cancelled);
        }
        match send_attempt(client, request, &options, signal.clone()).await {
            Ok(response) => {
                let should_retry = options.retry_on_status
                    && RETRYABLE_STATUS_CODES.contains(&response.status())
                    && attempt < options.max_retries;
                if !should_retry {
                    return Ok(response);
                }
                // Explicitly drop the response before retrying. This closes
                // the body/connection and mirrors the upstream body.cancel().
                drop(response);
            }
            Err(FetchRetryError::AttemptTimedOut) if attempt < options.max_retries => {}
            Err(error @ FetchRetryError::Transport(_)) if attempt < options.max_retries => {
                let _ = error;
            }
            Err(error) => return Err(error),
        }
        retry_delay(options.retry_delay, signal.clone()).await?;
    }
    unreachable!("the retry loop returns on the final attempt")
}

/// Execute one idempotent management request with bounded retry semantics.
///
/// `signal` is an optional process-local cancellation flag. Caller
/// cancellation and the overall timeout are terminal; an attempt timeout is
/// retryable while attempts remain.
pub async fn fetch_with_retry(
    client: &Client,
    request: &ManagementRequest,
    options: FetchRetryOptions,
    signal: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Response, FetchRetryError> {
    let timeout = options.timeout.filter(|duration| !duration.is_zero());
    match timeout {
        Some(timeout) => {
            tokio::time::timeout(timeout, fetch_loop(client, request, options, signal))
                .await
                .unwrap_or(Err(FetchRetryError::TimedOut))
        }
        None => fetch_loop(client, request, options, signal).await,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client() -> Client {
        Client::builder().no_proxy().build().expect("test client")
    }

    fn request(url: String) -> ManagementRequest {
        ManagementRequest::get(Url::parse(&url).expect("test URL"))
    }

    fn spawn_status_server(
        statuses: Vec<&'static str>,
    ) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
        spawn_status_server_with_delay(statuses, Duration::ZERO)
    }

    fn spawn_status_server_with_delay(
        statuses: Vec<&'static str>,
        delay: Duration,
    ) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let count = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&count);
        let thread = std::thread::spawn(move || {
            for status in statuses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                served.fetch_add(1, Ordering::SeqCst);
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let body = if status == "200 OK" { "ok" } else { "retry" };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{address}/management"), count, thread)
    }

    #[tokio::test]
    async fn retries_retryable_status_and_returns_final_response() {
        let (url, count, server) = spawn_status_server(vec!["500 Internal Server Error", "200 OK"]);
        let response = fetch_with_retry(
            &client(),
            &request(url),
            FetchRetryOptions {
                retry_delay: Duration::ZERO,
                ..Default::default()
            },
            None,
        )
        .await
        .expect("retry succeeds");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        server.join().expect("server thread");
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_status() {
        let (url, count, server) = spawn_status_server(vec!["400 Bad Request"]);
        let response = fetch_with_retry(
            &client(),
            &request(url),
            FetchRetryOptions {
                retry_delay: Duration::ZERO,
                ..Default::default()
            },
            None,
        )
        .await
        .expect("HTTP response is returned");
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.join().expect("server thread");
    }

    #[tokio::test]
    async fn caller_cancellation_is_terminal() {
        let url = "http://127.0.0.1:9/management".to_string();
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let error = fetch_with_retry(
            &client(),
            &request(url),
            FetchRetryOptions::default(),
            Some(signal),
        )
        .await
        .expect_err("cancelled request");
        assert!(matches!(error, FetchRetryError::Cancelled));
    }

    #[tokio::test]
    async fn attempt_timeout_is_retryable_but_final_timeout_is_reported() {
        let (url, count, server) =
            spawn_status_server_with_delay(vec!["200 OK", "200 OK"], Duration::from_millis(40));
        let error = fetch_with_retry(
            &client(),
            &request(url),
            FetchRetryOptions {
                max_retries: 1,
                attempt_timeout: Some(Duration::from_millis(5)),
                timeout: Some(Duration::from_millis(500)),
                retry_delay: Duration::ZERO,
                ..Default::default()
            },
            None,
        )
        .await
        .expect_err("both attempts time out");
        assert!(matches!(error, FetchRetryError::AttemptTimedOut));
        server.join().expect("server thread");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn overall_timeout_covers_an_in_flight_attempt() {
        let (url, count, server) =
            spawn_status_server_with_delay(vec!["200 OK"], Duration::from_millis(100));
        let error = fetch_with_retry(
            &client(),
            &request(url),
            FetchRetryOptions {
                max_retries: 0,
                timeout: Some(Duration::from_millis(5)),
                ..Default::default()
            },
            None,
        )
        .await
        .expect_err("overall timeout must settle the request");
        assert!(matches!(error, FetchRetryError::TimedOut));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.join().expect("server thread");
    }

    #[tokio::test]
    async fn caller_cancellation_interrupts_an_in_flight_attempt() {
        let (url, count, server) =
            spawn_status_server_with_delay(vec!["200 OK"], Duration::from_millis(100));
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal_for_task = Arc::clone(&signal);
        let task = tokio::spawn(async move {
            fetch_with_retry(
                &client(),
                &request(url),
                FetchRetryOptions {
                    max_retries: 0,
                    ..Default::default()
                },
                Some(signal_for_task),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        signal.store(true, Ordering::Release);
        let error = task
            .await
            .expect("request task joins")
            .expect_err("cancelled request");
        assert!(matches!(error, FetchRetryError::Cancelled));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.join().expect("server thread");
    }
}
