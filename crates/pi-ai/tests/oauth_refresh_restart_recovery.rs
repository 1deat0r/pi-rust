#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panicking assertions are the point

//! Focused OpenAI Codex OAuth refresh/recovery coverage.
//!
//! Every HTTP exchange in this file is loopback-only and uses synthetic
//! credentials.  The fixtures intentionally assert that token-shaped values
//! never appear in returned diagnostics; they are never logged.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::auth::{
    classify_oauth_failure, AuthEvent, AuthInteraction, AuthPrompt, Credential, CredentialStore,
    InMemoryCredentialStore, OAuthAuth, OAuthCredential, OAuthFailureKind,
};
use pi_ai::models::{create_models, CreateModelsOptions};
use pi_ai::oauth::{base64url_encode, parse_openai_codex_authorization_input, OpenAICodexOAuth};
use pi_ai::providers::all::openai_codex_provider_with_oauth;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const REFRESH_TOKEN: &str = "fixture-refresh-token";
const ROTATED_REFRESH_TOKEN: &str = "fixture-rotated-refresh-token";

struct MockResponse {
    status: u16,
    body: String,
    delay: Duration,
}

impl MockResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }

    fn delayed_json(status: u16, body: Value, delay: Duration) -> Self {
        Self {
            status,
            body: body.to_string(),
            delay,
        }
    }
}

struct MockState {
    responses: Mutex<VecDeque<MockResponse>>,
    token_requests: AtomicUsize,
    active_token_requests: AtomicUsize,
    max_active_token_requests: AtomicUsize,
}

struct MockServer {
    base_url: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(responses: impl IntoIterator<Item = MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(MockState {
            responses: Mutex::new(responses.into_iter().collect()),
            token_requests: AtomicUsize::new(0),
            active_token_requests: AtomicUsize::new(0),
            max_active_token_requests: AtomicUsize::new(0),
        });
        let state_for_task = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let state = state_for_task.clone();
                tokio::spawn(async move {
                    handle_connection(socket, state).await;
                });
            }
        });
        Self {
            base_url,
            state,
            task,
        }
    }

    fn token_requests(&self) -> usize {
        self.state.token_requests.load(Ordering::SeqCst)
    }

    fn max_active_token_requests(&self) -> usize {
        self.state.max_active_token_requests.load(Ordering::SeqCst)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_connection(mut socket: TcpStream, state: Arc<MockState>) {
    let mut buffer = [0u8; 16 * 1024];
    let read = socket.read(&mut buffer).await.unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");

    let is_token_request = path == "/oauth/token";
    if is_token_request {
        state.token_requests.fetch_add(1, Ordering::SeqCst);
        let active = state.active_token_requests.fetch_add(1, Ordering::SeqCst) + 1;
        update_max(&state.max_active_token_requests, active);
    }

    let response = if is_token_request {
        state
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| MockResponse::json(500, json!({"error": "fixture exhausted"})))
    } else {
        MockResponse::json(404, json!({"error": "not found"}))
    };
    tokio::time::sleep(response.delay).await;
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let wire = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.body.len(),
        response.body
    );
    let _ = socket.write_all(wire.as_bytes()).await;
    if is_token_request {
        state.active_token_requests.fetch_sub(1, Ordering::SeqCst);
    }
}

fn update_max(maximum: &AtomicUsize, value: usize) {
    let mut current = maximum.load(Ordering::SeqCst);
    while value > current {
        match maximum.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(updated) => current = updated,
        }
    }
}

fn fake_jwt(account_id: &str) -> String {
    let payload = json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "e30.{}.sig",
        base64url_encode(serde_json::to_string(&payload).unwrap().as_bytes())
    )
}

fn near_expiry_credential() -> OAuthCredential {
    OAuthCredential {
        refresh: REFRESH_TOKEN.to_string(),
        access: fake_jwt("fixture-old-account"),
        expires: pi_ai::types::now_ms().saturating_add(1_000),
        extra: [("accountId".to_string(), json!("fixture-old-account"))]
            .into_iter()
            .collect(),
    }
}

fn successful_token_response(account_id: &str) -> MockResponse {
    MockResponse::json(
        200,
        json!({
            "access_token": fake_jwt(account_id),
            "refresh_token": ROTATED_REFRESH_TOKEN,
            "expires_in": 3600
        }),
    )
}

fn assert_redacted(error: &str, secrets: &[&str]) {
    for secret in secrets {
        assert!(!error.to_string().contains(secret));
    }
}

#[tokio::test]
async fn near_expiry_refresh_retries_one_transient_failure_and_rotates_credential() {
    let credential = near_expiry_credential();
    let old_access = credential.access.clone();
    let server = MockServer::start([
        MockResponse::json(
            503,
            json!({
                "error": "temporarily_unavailable",
                "error_description": format!(
                    "temporary refresh={REFRESH_TOKEN} access={old_access}"
                )
            }),
        ),
        successful_token_response("fixture-new-account"),
    ])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", 0);
    let signal = AtomicBool::new(false);

    let refreshed = oauth.refresh(&credential, &signal).await.unwrap();

    assert_eq!(server.token_requests(), 2);
    assert!(refreshed.refresh == ROTATED_REFRESH_TOKEN);
    assert!(refreshed.access == fake_jwt("fixture-new-account"));
    assert!(refreshed.expires > credential.expires);
    assert_eq!(
        classify_oauth_failure("transient fixture failure [server]"),
        OAuthFailureKind::Server
    );
}

#[tokio::test]
async fn invalid_grant_is_classified_actionably_without_a_retry_or_secret_echo() {
    let credential = near_expiry_credential();
    let old_access = credential.access.clone();
    let server = MockServer::start([MockResponse::json(
        400,
        json!({
            "error": "invalid_grant",
            "error_description": format!(
                "refresh token rejected: {REFRESH_TOKEN}; access={old_access}"
            )
        }),
    )])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", 0);
    let signal = AtomicBool::new(false);

    let error = oauth.refresh(&credential, &signal).await.unwrap_err();
    let error = error.to_string();

    assert_eq!(server.token_requests(), 1);
    assert_eq!(
        classify_oauth_failure(&error.to_string()),
        OAuthFailureKind::InvalidGrant
    );
    assert!(error.to_string().contains("/login openai-codex"));
    assert_redacted(&error.to_string(), &[REFRESH_TOKEN, &old_access]);
}

#[tokio::test]
async fn malformed_jwt_account_extraction_is_non_retryable_and_redacted() {
    let credential = near_expiry_credential();
    let malformed_access = "malformed-access-token";
    let server = MockServer::start([MockResponse::json(
        200,
        json!({
            "access_token": malformed_access,
            "refresh_token": ROTATED_REFRESH_TOKEN,
            "expires_in": 3600
        }),
    )])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", 0);
    let signal = AtomicBool::new(false);

    let error = oauth.refresh(&credential, &signal).await.unwrap_err();
    let error = error.to_string();

    assert_eq!(server.token_requests(), 1);
    assert_eq!(
        classify_oauth_failure(&error.to_string()),
        OAuthFailureKind::AccountExtraction
    );
    assert!(error.to_string().contains("accountId"));
    assert!(error.to_string().contains("/login openai-codex"));
    assert_redacted(&error.to_string(), &[malformed_access, REFRESH_TOKEN]);
}

#[tokio::test]
async fn malformed_token_response_is_classified_without_retry() {
    let credential = near_expiry_credential();
    let server = MockServer::start([MockResponse::json(
        200,
        json!({"access_token": "malformed-response-access"}),
    )])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", 0);
    let signal = AtomicBool::new(false);

    let error = oauth.refresh(&credential, &signal).await.unwrap_err();
    let error = error.to_string();

    assert_eq!(server.token_requests(), 1);
    assert_eq!(
        classify_oauth_failure(&error.to_string()),
        OAuthFailureKind::MalformedResponse
    );
    assert!(error.to_string().contains("/login openai-codex"));
}

#[tokio::test]
async fn concurrent_refreshes_for_one_credential_are_serialized() {
    let credential = near_expiry_credential();
    let server = MockServer::start([
        MockResponse::delayed_json(
            200,
            json!({
                "access_token": fake_jwt("fixture-concurrent-one"),
                "refresh_token": "fixture-concurrent-refresh-one",
                "expires_in": 3600
            }),
            Duration::from_millis(80),
        ),
        MockResponse::delayed_json(
            200,
            json!({
                "access_token": fake_jwt("fixture-concurrent-two"),
                "refresh_token": "fixture-concurrent-refresh-two",
                "expires_in": 3600
            }),
            Duration::from_millis(80),
        ),
    ])
    .await;
    let oauth = OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", 0);
    let first_signal = AtomicBool::new(false);
    let second_signal = AtomicBool::new(false);

    let (first, second) = tokio::join!(
        oauth.refresh(&credential, &first_signal),
        oauth.refresh(&credential, &second_signal)
    );

    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(server.token_requests(), 2);
    assert_eq!(server.max_active_token_requests(), 1);
}

#[derive(Clone, Copy)]
enum BrowserAnswer {
    Redirect,
    AuthorizationError,
}

struct BrowserInteraction {
    answer: BrowserAnswer,
    events: Arc<Mutex<Vec<AuthEvent>>>,
}

impl BrowserInteraction {
    fn new(answer: BrowserAnswer) -> Self {
        Self {
            answer,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn manual_input(&self) -> String {
        let state = self
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                AuthEvent::AuthUrl { url, .. } => url::Url::parse(url).ok().and_then(|url| {
                    url.query_pairs()
                        .find(|(key, _)| key == "state")
                        .map(|(_, value)| value.into_owned())
                }),
                _ => None,
            })
            .unwrap();
        let mut callback = url::Url::parse("http://localhost/auth/callback").unwrap();
        match self.answer {
            BrowserAnswer::Redirect => {
                callback
                    .query_pairs_mut()
                    .append_pair("code", "fixture-manual-code")
                    .append_pair("state", &state);
            }
            BrowserAnswer::AuthorizationError => {
                callback
                    .query_pairs_mut()
                    .append_pair("error", "access_denied")
                    .append_pair(
                        "error_description",
                        "authorization-code-secret-must-not-echo",
                    )
                    .append_pair("state", &state);
            }
        }
        callback.to_string()
    }
}

impl AuthInteraction for BrowserInteraction {
    fn prompt(&self, prompt: &AuthPrompt) -> Result<String, String> {
        match prompt {
            AuthPrompt::Select { .. } => Ok("browser".to_string()),
            _ => Ok(self.manual_input()),
        }
    }

    fn notify(&self, event: &AuthEvent) {
        self.events.lock().unwrap().push(event.clone());
    }

    fn prompt_async_with_abort<'a>(
        &'a self,
        _prompt: &'a AuthPrompt,
        abort: Arc<AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        let input = self.manual_input();
        Box::pin(async move {
            if abort.load(Ordering::SeqCst) {
                return Err("Login cancelled".to_string());
            }
            Ok(input)
        })
    }
}

async fn reserve_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

#[tokio::test]
async fn browser_login_uses_manual_redirect_when_loopback_port_is_occupied() {
    let server = MockServer::start([successful_token_response("fixture-browser-account")]).await;
    let (blocker, callback_port) = reserve_port().await;
    let oauth =
        OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", callback_port);
    let interaction = BrowserInteraction::new(BrowserAnswer::Redirect);

    let credential = oauth.login(&interaction).await.unwrap();
    drop(blocker);

    assert!(credential.extra.contains_key("accountId"));
    assert_eq!(server.token_requests(), 1);
}

#[tokio::test]
async fn browser_manual_authorization_errors_are_actionable_and_do_not_echo_input() {
    let server = MockServer::start([]).await;
    let (blocker, callback_port) = reserve_port().await;
    let oauth =
        OpenAICodexOAuth::with_base_url_and_callback(&server.base_url, "127.0.0.1", callback_port);
    let interaction = BrowserInteraction::new(BrowserAnswer::AuthorizationError);

    let error = oauth.login(&interaction).await.unwrap_err();
    drop(blocker);

    assert_eq!(
        classify_oauth_failure(&error.to_string()),
        OAuthFailureKind::Protocol
    );
    assert!(error.to_string().contains("/login openai-codex"));
    assert_redacted(
        &error.to_string(),
        &["authorization-code-secret-must-not-echo"],
    );
    assert_eq!(server.token_requests(), 0);
}

#[test]
fn authorization_error_query_is_not_treated_as_a_raw_code() {
    let parsed = parse_openai_codex_authorization_input(
        "http://localhost/auth/callback?error=access_denied&error_description=do-not-echo",
    );
    assert!(parsed.code.is_none());
    assert!(parsed.state.is_none());
}

#[test]
fn persisted_oauth_credential_survives_restart_and_logout_allows_relogin() {
    let first = OAuthCredential {
        refresh: "fixture-restart-refresh".to_string(),
        access: "fixture-restart-access".to_string(),
        expires: pi_ai::types::now_ms().saturating_add(3_600_000),
        extra: [("accountId".to_string(), json!("fixture-restart-account"))]
            .into_iter()
            .collect(),
    };
    let persistent = Arc::new(InMemoryCredentialStore::new());
    persistent.modify("openai-codex", &|_| Some(Credential::OAuth(first.clone())));

    let encoded = serde_json::to_value(persistent.read("openai-codex").unwrap()).unwrap();
    let restarted = Arc::new(InMemoryCredentialStore::new());
    let restored: Credential = serde_json::from_value(encoded).unwrap();
    restarted.modify("openai-codex", &|_| Some(restored.clone()));

    let oauth = OpenAICodexOAuth::with_base_url_and_callback("http://127.0.0.1:1", "127.0.0.1", 0);
    let models = create_models(CreateModelsOptions {
        credentials: Some(restarted.clone()),
        ..Default::default()
    });
    models.set_provider(openai_codex_provider_with_oauth(oauth));

    assert!(models.get_auth("openai-codex", None).is_some_and(|result| {
        result.auth.api_key.as_deref() == Some("fixture-restart-access")
    }));

    restarted.delete("openai-codex");
    assert!(models.get_auth("openai-codex", None).is_none());

    let relogin = OAuthCredential {
        refresh: "fixture-relogin-refresh".to_string(),
        access: "fixture-relogin-access".to_string(),
        expires: pi_ai::types::now_ms().saturating_add(3_600_000),
        extra: [("accountId".to_string(), json!("fixture-relogin-account"))]
            .into_iter()
            .collect(),
    };
    restarted.modify("openai-codex", &|_| {
        Some(Credential::OAuth(relogin.clone()))
    });
    assert!(models.get_auth("openai-codex", None).is_some_and(|result| {
        result.auth.api_key.as_deref() == Some("fixture-relogin-access")
    }));
}
